use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    time::Instant,
};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    explain_analyze_statement, explain_statement,
};
use pintail_meta::{DatabaseRecord, MetaStore, TableRecord};
use pintail_probe::{ProbeReport, SourceTable};
use pintail_sql::{Binder, MetadataError, Statement, execute_metadata, parse_statement};
use pintail_store::TableSnapshot;
use pintail_types::{DataType, Value};
use thiserror::Error;

/// Default hard memory ceiling for one client query.
pub const DEFAULT_QUERY_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Default result row ceiling for HTTP and wire clients.
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// A query output field in `MySQL` presentation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryField {
    pub name: String,
    pub data_type: Option<DataType>,
    pub nullable: bool,
}

/// Physical work observed while executing one query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryStats {
    pub duration_ms: u64,
    pub rows: usize,
    pub batches: usize,
    pub segments_read: usize,
    pub segments_pruned: usize,
    pub blocks_read: usize,
    pub blocks_pruned: usize,
    pub blocks_decoded: usize,
}

/// Typed, bounded result returned by Pintail's shared query service.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryOutput {
    pub fields: Vec<QueryField>,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
    pub truncated: bool,
}

/// Failure from loading or querying one mirrored database.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QueryError {
    #[error("database does not exist")]
    DatabaseNotFound,
    #[error("replica is not ready: {0}")]
    NotReady(String),
    #[error("{0}")]
    Invalid(String),
    #[error("query engine failed: {0}")]
    Internal(String),
}

/// Opens reader-pinned table snapshots and runs Pintail's native SQL engine.
#[derive(Clone, Debug)]
pub struct ReplicaEngine {
    data_dir: PathBuf,
    metadata_path: PathBuf,
    memory_limit: usize,
}

struct LoadedReplica {
    database: DatabaseRecord,
    tables: Vec<TableRecord>,
    targets: Vec<ReaderTarget>,
}

struct ReaderTarget {
    source: SourceTable,
    snapshot: TableSnapshot,
}

impl ReplicaEngine {
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>, metadata_path: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            metadata_path: metadata_path.into(),
            memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        }
    }

    #[must_use]
    pub const fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// Executes one read-only MySQL-dialect statement.
    ///
    /// # Errors
    ///
    /// Returns an error when the database is absent or unready, the statement
    /// is invalid or mutating, or storage/execution fails.
    pub fn execute(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
    ) -> Result<QueryOutput, QueryError> {
        let started = Instant::now();
        let replica = self.load_replica(database_id)?;
        let catalog = build_catalog(&replica)?;
        let mut provider = build_provider(&replica)?;
        let table_count = replica.targets.len();
        let statement =
            parse_statement(sql).map_err(|error| QueryError::Invalid(error.to_string()))?;
        match execute_metadata(&statement, &catalog, Some(&replica.database.name)) {
            Ok(result) => return Ok(metadata_output(result, started)),
            Err(MetadataError::Unsupported(_)) => {}
            Err(error) => return Err(QueryError::Invalid(error.to_string())),
        }
        match statement {
            Statement::Query(_) => self.execute_select(
                &statement,
                &catalog,
                &provider,
                &replica.database.name,
                table_count,
                started,
                max_rows,
            ),
            Statement::Explain { .. } => self.execute_explain(
                &statement,
                &catalog,
                &mut provider,
                &replica.database.name,
                table_count,
                started,
            ),
            _ => Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_select(
        &self,
        statement: &Statement,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
        database_name: &str,
        table_count: usize,
        started: Instant,
        max_rows: usize,
    ) -> Result<QueryOutput, QueryError> {
        let bound = Binder::new(catalog, Some(database_name))
            .bind(statement)
            .map_err(|error| QueryError::Invalid(error.to_string()))?;
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical)
            .map_err(|error| QueryError::Invalid(error.to_string()))?;
        let mut execution = Execution::start(physical, provider, self.memory_limit)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let fields = execution
            .output_fields()
            .iter()
            .map(|field| QueryField {
                name: field.name.clone(),
                data_type: field.data_type,
                nullable: field.nullable,
            })
            .collect();
        let (rows, batches, truncated) = collect_rows(&mut execution, max_rows)?;
        let mut stats = provider_stats(provider, table_count);
        stats.duration_ms = elapsed_ms(started);
        stats.rows = rows.len();
        stats.batches = batches;
        Ok(QueryOutput {
            fields,
            rows,
            stats,
            truncated,
        })
    }

    fn execute_explain(
        &self,
        statement: &Statement,
        catalog: &CatalogSnapshot,
        provider: &mut SnapshotScanProvider<'_>,
        database_name: &str,
        table_count: usize,
        started: Instant,
    ) -> Result<QueryOutput, QueryError> {
        let plan = explain_statement(statement, catalog, Some(database_name)).or_else(|_| {
            explain_analyze_statement(
                statement,
                catalog,
                Some(database_name),
                provider,
                self.memory_limit,
            )
        });
        let plan = plan.map_err(|error| QueryError::Invalid(error.to_string()))?;
        let mut stats = provider_stats(provider, table_count);
        stats.duration_ms = elapsed_ms(started);
        stats.rows = 1;
        Ok(QueryOutput {
            fields: vec![QueryField {
                name: "plan".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: false,
            }],
            rows: vec![vec![Value::Utf8(plan)]],
            stats,
            truncated: false,
        })
    }

    fn load_replica(&self, database_id: &str) -> Result<LoadedReplica, QueryError> {
        let metadata = MetaStore::open(&self.metadata_path)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let database = metadata
            .database(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?
            .ok_or(QueryError::DatabaseNotFound)?;
        let report: ProbeReport = serde_json::from_str(
            database
                .probe_json
                .as_deref()
                .ok_or_else(|| QueryError::NotReady("database has not been probed".to_owned()))?,
        )
        .map_err(|error| QueryError::Internal(error.to_string()))?;
        let tables = metadata
            .tables(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let table_records = tables
            .iter()
            .map(|table| (table.name.to_ascii_lowercase(), table))
            .collect::<BTreeMap<_, _>>();
        let root = self
            .data_dir
            .join("databases")
            .join(database_id)
            .join("tables");
        let targets = report
            .tables
            .into_iter()
            .filter(|source| table_records.contains_key(&source.name.to_ascii_lowercase()))
            .map(|mut source| {
                let history = metadata
                    .schema_history(database_id, &source.name)
                    .map_err(|error| QueryError::Internal(error.to_string()))?;
                let version = history.last().map_or(1, |record| record.version);
                if let Some(record) = history.last() {
                    source.columns = serde_json::from_str(&record.columns_json)
                        .map_err(|error| QueryError::Internal(error.to_string()))?;
                }
                let schema = source
                    .table_schema_with_version(version)
                    .map_err(|error| QueryError::Internal(error.to_string()))?;
                let directory = table_directory(&root, &source.name);
                let snapshot = TableSnapshot::open(directory, schema)
                    .map_err(|error| QueryError::NotReady(error.to_string()))?;
                Ok(ReaderTarget { source, snapshot })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LoadedReplica {
            database,
            tables,
            targets,
        })
    }
}

fn collect_rows(
    execution: &mut Execution,
    max_rows: usize,
) -> Result<(Vec<Vec<Value>>, usize, bool), QueryError> {
    let mut rows = Vec::new();
    let mut batches = 0;
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| QueryError::Internal(error.to_string()))?
    {
        batches += 1;
        for row in batch.selection().selected_rows() {
            if rows.len() == max_rows {
                return Ok((rows, batches, true));
            }
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or_else(|| {
                        QueryError::Internal("query batch has a missing value".to_owned())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
    }
    Ok((rows, batches, false))
}

fn metadata_output(result: pintail_sql::MetadataResult, started: Instant) -> QueryOutput {
    QueryOutput {
        fields: result
            .fields
            .into_iter()
            .map(|field| QueryField {
                name: field.name,
                data_type: Some(field.data_type),
                nullable: field.nullable,
            })
            .collect(),
        stats: QueryStats {
            duration_ms: elapsed_ms(started),
            rows: result.rows.len(),
            ..QueryStats::default()
        },
        rows: result.rows,
        truncated: false,
    }
}

fn build_catalog(replica: &LoadedReplica) -> Result<CatalogSnapshot, QueryError> {
    let row_counts = replica
        .tables
        .iter()
        .map(|table| (table.name.to_ascii_lowercase(), table.rows_synced))
        .collect::<BTreeMap<_, _>>();
    let entries = replica
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let id = table_id(index)?;
            let rows = row_counts
                .get(&target.source.name.to_ascii_lowercase())
                .copied()
                .or(target.source.estimated_rows)
                .unwrap_or(0);
            let entry = TableEntry::new(
                id,
                &target.source.name,
                target.snapshot.schema().clone(),
                TableStatistics::with_row_count(rows),
            )
            .map_err(|error| QueryError::Internal(error.to_string()))?;
            let key_columns = target.source.key_column_ids();
            if key_columns.is_empty() {
                Ok(entry)
            } else {
                entry
                    .with_key_columns(key_columns)
                    .map_err(|error| QueryError::Internal(error.to_string()))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let database = DatabaseEntry::new(DatabaseId::new(1), &replica.database.name, entries)
        .map_err(|error| QueryError::Internal(error.to_string()))?;
    CatalogSnapshot::new([database]).map_err(|error| QueryError::Internal(error.to_string()))
}

fn build_provider(replica: &LoadedReplica) -> Result<SnapshotScanProvider<'_>, QueryError> {
    let database_id = DatabaseId::new(1);
    let indexed = replica
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| Ok((database_id, table_id(index)?, &target.snapshot)))
        .collect::<Result<Vec<_>, QueryError>>()?;
    let mut provider = SnapshotScanProvider::new(indexed)
        .map_err(|error| QueryError::Internal(error.to_string()))?;
    for (index, target) in replica.targets.iter().enumerate() {
        let storage_key = target.source.key_column_ids();
        let unique_keys = target
            .source
            .unique_keys
            .iter()
            .map(|key| {
                key.iter()
                    .filter_map(|name| {
                        target
                            .source
                            .columns
                            .iter()
                            .find(|column| column.name.eq_ignore_ascii_case(name))
                            .map(|column| column.id)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|key| !key.is_empty())
            .filter(|key| *key != storage_key)
            .collect::<Vec<_>>();
        if !unique_keys.is_empty() {
            provider
                .enable_unique_visibility_policy(database_id, table_id(index)?, unique_keys)
                .map_err(|error| QueryError::Internal(error.to_string()))?;
        }
    }
    Ok(provider)
}

fn provider_stats(provider: &SnapshotScanProvider<'_>, table_count: usize) -> QueryStats {
    let mut output = QueryStats::default();
    for index in 0..table_count {
        let Ok(table_id) = table_id(index) else {
            break;
        };
        let Some(stats) = provider.scan_stats(DatabaseId::new(1), table_id) else {
            continue;
        };
        output.segments_read += stats.segments_read;
        output.segments_pruned += stats.segments_pruned;
        output.blocks_read += stats.blocks_read;
        output.blocks_pruned += stats.blocks_pruned;
        output.blocks_decoded += stats.blocks_decoded;
    }
    output
}

fn table_id(index: usize) -> Result<TableId, QueryError> {
    let id = u64::try_from(index)
        .ok()
        .and_then(|index| index.checked_add(1))
        .ok_or_else(|| QueryError::Internal("table catalog ID overflow".to_owned()))?;
    Ok(TableId::new(id))
}

/// Returns the stable on-disk directory for one source table.
#[must_use]
pub fn table_directory(root: &Path, table: &str) -> PathBuf {
    let safe = table
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let mut hasher = DefaultHasher::new();
    table.to_ascii_lowercase().hash(&mut hasher);
    root.join(format!("table-{safe}-{:016x}", hasher.finish()))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
