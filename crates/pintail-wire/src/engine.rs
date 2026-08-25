use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    ExecError, Execution, ExplainError, LogicalPlanner, Optimizer, PhysicalPlanner,
    SnapshotScanProvider, explain_analyze_statement_with_deadline, explain_statement,
};
use pintail_meta::{DatabaseRecord, MetaStore, TableRecord};

use crate::admission::{QueryAdmission, shared_admission};
use pintail_probe::{ProbeReport, SourceTable};
use pintail_sql::{
    Binder, BoundExprKind, BoundJoinKind, BoundQuery, ColumnFacts, DEFAULT_TEXT_COLLATION,
    IndexFacts, MetadataError, SourceFacts, Statement, execute_metadata, parse_statement,
};
use pintail_store::TableSnapshot;
use pintail_types::{DataType, Value};
use thiserror::Error;

/// Default hard memory ceiling for one client query.
///
/// Sized for an analytical join rather than a point lookup. At 64MiB a
/// nine-way dashboard join over a four-thousand-row table was refused - not a
/// pathological query, just the shape a health or funnel report takes - and
/// the operator's only signal was a byte count. Operators spill rather than
/// fail above this, so a larger ceiling trades resident memory for fewer
/// spills; the concurrent total, not this, is what bounds the process.
pub const DEFAULT_QUERY_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
/// Default result row ceiling for HTTP and wire clients.
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// A query output field in `MySQL` presentation order.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // independent per-column wire facts
pub struct QueryField {
    pub name: String,
    pub data_type: Option<DataType>,
    pub nullable: bool,
    /// Resolved text collation, absent for non-text results.
    pub collation: Option<String>,
    /// Direct `GROUP_CONCAT` projections choose VARCHAR versus TEXT/BLOB on
    /// the wire from the connection's `group_concat_max_len`.
    pub group_concat: bool,
    /// Spatial column: advertised as `MYSQL_TYPE_GEOMETRY` on the wire.
    pub geometry: bool,
    /// Source `TIMESTAMP` column: advertised as `MYSQL_TYPE_TIMESTAMP`.
    pub timestamp: bool,
    /// Wire-metadata override for direct projections whose VALUES stay
    /// variable-width text deliberately (`SEC_TO_TIME`'s fraction follows
    /// its input), but whose column TYPE matches `MySQL`'s.
    pub wire_hint: Option<WireTypeHint>,
}

/// The column type `MySQL` advertises for a handful of text-carried results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireTypeHint {
    /// `SEC_TO_TIME`/`MAKETIME`: `MYSQL_TYPE_TIME`.
    Time,
    /// `CONVERT_TZ`: `MYSQL_TYPE_DATETIME`.
    Datetime,
    /// `JSON_UNQUOTE`/`->>`: `MYSQL_TYPE_BLOB` with the binary collation.
    JsonText,
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
    /// Rows a WRITE changed, when the statement changed rows instead of
    /// returning them. `None` is a result set - every read answers `None`,
    /// so a query can never be mistaken for a write - and `Some` makes the
    /// server answer with an OK packet carrying this count.
    pub affected: Option<u64>,
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
    /// A statement the engine understood and rejected for a reason `MySQL`
    /// names with a specific error code - kept apart from
    /// [`QueryError::Invalid`] so the wire server can answer with `MySQL`'s
    /// errno/SQLSTATE instead of a blanket parse error.
    #[error("{message}")]
    Rejected {
        /// Which `MySQL` error class the rejection belongs to.
        rejection: SqlRejection,
        /// Human-readable detail.
        message: String,
    },
    #[error("query engine failed: {0}")]
    Internal(String),
    #[error("query execution was interrupted after max_execution_time elapsed")]
    Interrupted,
    #[error("too many concurrent queries; the server is at its execution limit, retry shortly")]
    Overloaded,
}

/// The `MySQL` error classes Pintail distinguishes on the wire. Each maps
/// to one errno/SQLSTATE pair; everything else stays a 1064 parse error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlRejection {
    /// 1049: the qualified database does not exist.
    UnknownDatabase,
    /// 1146: the table does not exist.
    UnknownTable,
    /// 1054: the column (or relation qualifier) does not exist.
    UnknownColumn,
    /// 1052: an unqualified name matches more than one input.
    AmbiguousColumn,
    /// 1055: a selected column is neither grouped nor aggregated.
    UngroupedColumn,
    /// 1111: a group function appeared where no aggregation scope exists.
    GroupFunctionMisplaced,
    /// 1690: numeric evaluation left the result type's range.
    OutOfRange,
    /// 1050: `CREATE TABLE` named an existing table.
    TableExists,
    /// 1062: a write repeated a unique key.
    DuplicateKey,
    /// 1048: a `NOT NULL` column received no value.
    NotNull,
}

/// Opens reader-pinned table snapshots and runs Pintail's native SQL engine.
#[derive(Clone)]
pub struct ReplicaEngine {
    data_dir: PathBuf,
    metadata_path: PathBuf,
    memory_limit: usize,
    /// Bounds concurrent execution. Without it the server admits every
    /// query and converts overload into unbounded latency rather than
    /// backpressure (see `tests/load/results.md`).
    admission: std::sync::Arc<QueryAdmission>,
    /// Loaded replicas keyed by database, revalidated per request against
    /// on-disk file stamps: reopening every table snapshot (manifest read
    /// plus WAL merge) and the metadata store cost ~200ms on EVERY query,
    /// the fixed floor under the whole benchmark board.
    cache: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, CachedReplica>>>,
}

impl std::fmt::Debug for ReplicaEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplicaEngine")
            .field("data_dir", &self.data_dir)
            .field("metadata_path", &self.metadata_path)
            .field("memory_limit", &self.memory_limit)
            .finish_non_exhaustive()
    }
}

struct CachedReplica {
    stamp: Vec<(PathBuf, u64, Option<std::time::SystemTime>)>,
    replica: std::sync::Arc<LoadedReplica>,
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
            admission: shared_admission(),
            cache: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Every file whose content can change what a query sees: the metadata
    /// store plus each table directory's entries (manifests, WALs and the
    /// immutable segment set). Any CDC apply, flush, compaction or schema
    /// change alters at least one (path, len, mtime) triple.
    fn replica_stamp(
        &self,
        database_id: &str,
    ) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
        let mut stamp = Vec::new();
        let mut record = |path: &Path| {
            if let Ok(meta) = std::fs::metadata(path) {
                stamp.push((path.to_path_buf(), meta.len(), meta.modified().ok()));
            }
        };
        record(&self.metadata_path);
        // Metadata writes land in SQLite's WAL, not the main file — without
        // it a replica cached between a table's files appearing and its
        // metadata rows committing stays stale until unrelated data churn.
        let mut wal = self.metadata_path.as_os_str().to_owned();
        wal.push("-wal");
        record(Path::new(&wal));
        let tables_root = self
            .data_dir
            .join("databases")
            .join(database_id)
            .join("tables");
        let mut directories = vec![tables_root];
        while let Some(directory) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    directories.push(path);
                } else {
                    record(&path);
                }
            }
        }
        stamp
    }

    fn load_replica_cached(
        &self,
        database_id: &str,
    ) -> Result<std::sync::Arc<LoadedReplica>, QueryError> {
        // Every query pays this before it plans anything, and on a miss it
        // pays a reload of EVERY table's store - so on a replica under
        // active CDC, where any commit changes the stamp, a trivial query
        // can cost more in setup than in execution. That is invisible
        // without a number, and it is the number to ask an operator for
        // when a cheap query is inexplicably slow.
        let stamp_started = Instant::now();
        let stamp = self.replica_stamp(database_id);
        let stamped = stamp_started.elapsed();
        if let Some(cached) = self
            .cache
            .lock()
            .expect("replica cache lock")
            .get(database_id)
            .filter(|cached| cached.stamp == stamp)
        {
            pintail_log::log_debug!(
                "query setup db={database_id} stamp={:.1}ms files={} replica=cached",
                stamped.as_secs_f64() * 1_000.0,
                stamp.len()
            );
            return Ok(std::sync::Arc::clone(&cached.replica));
        }
        let load_started = Instant::now();
        let replica = std::sync::Arc::new(self.load_replica(database_id)?);
        pintail_log::log_debug!(
            "query setup db={database_id} stamp={:.1}ms files={} replica=reloaded in {:.1}ms tables={}",
            stamped.as_secs_f64() * 1_000.0,
            stamp.len(),
            load_started.elapsed().as_secs_f64() * 1_000.0,
            replica.targets.len()
        );
        self.cache.lock().expect("replica cache lock").insert(
            database_id.to_owned(),
            CachedReplica {
                stamp,
                replica: std::sync::Arc::clone(&replica),
            },
        );
        Ok(replica)
    }

    #[must_use]
    pub const fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// Bounds concurrent query execution. Zero is unbounded.
    #[must_use]
    pub fn with_max_concurrent_queries(mut self, limit: usize) -> Self {
        self.admission = std::sync::Arc::new(QueryAdmission::new(limit));
        self
    }

    /// The configured concurrency ceiling; zero means unbounded.
    #[must_use]
    pub fn max_concurrent_queries(&self) -> usize {
        self.admission.limit()
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
        self.execute_with_deadline(database_id, sql, max_rows, None)
    }

    /// Executes one statement with an optional monotonic deadline.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute`], plus
    /// [`QueryError::Interrupted`] when the deadline elapses.
    pub fn execute_with_deadline(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        let started = Instant::now();
        // Held for the whole execution and released on drop, including on
        // an early return or panic. Taken before any replica or catalog
        // work so a saturated server refuses cheaply.
        let _permit = self.admission.try_admit().ok_or(QueryError::Overloaded)?;
        let statement =
            parse_statement(sql).map_err(|error| QueryError::Invalid(error.to_string()))?;
        // Writes are routed before any replica is loaded: a write needs no
        // catalog snapshot, and a local database that has not created its
        // first table has none to load.
        if matches!(statement, Statement::CreateTable(_) | Statement::Insert(_)) {
            return self.execute_write(database_id, &statement, started);
        }
        let replica = self.load_replica_cached(database_id)?;
        let catalog = build_catalog(&replica)?;
        let mut provider = build_provider(&replica)?;
        let table_count = replica.targets.len();
        // `/*+ MAX_EXECUTION_TIME(ms) */` is scoped to the statement and
        // tightens whatever the session already allows - never loosens it, so
        // a hint cannot be used to escape an administrator's ceiling. A hint
        // of 0 means "no ceiling" in MySQL and simply leaves the session's in
        // force.
        let deadline = match pintail_sql::max_execution_time_hint(&statement) {
            Some(milliseconds) if milliseconds > 0 => Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .map(|hinted| deadline.map_or(hinted, |held| held.min(hinted)))
                .or(deadline),
            _ => deadline,
        };
        let facts = column_facts(&replica);
        match execute_metadata(&statement, &catalog, Some(&replica.database.name), &facts) {
            Ok(result) => return Ok(metadata_output(result, started)),
            Err(MetadataError::Unsupported(_)) => {}
            Err(error) => return Err(QueryError::Invalid(error.to_string())),
        }
        match statement {
            Statement::Query(_) => self.execute_select(
                &statement,
                &catalog,
                &provider,
                &facts,
                &replica.database.name,
                table_count,
                started,
                max_rows,
                deadline,
            ),
            Statement::Explain { .. } => self.execute_explain(
                &statement,
                &catalog,
                &mut provider,
                &replica.database.name,
                table_count,
                started,
                deadline,
            ),
            _ => Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            )),
        }
    }

    /// Executes one mutating statement against a LOCAL database.
    ///
    /// Replicated databases keep the read-only rejection: a row written
    /// into a mirrored table would be destroyed by the next resnapshot and
    /// has no binlog version it could legitimately claim
    /// (`docs/design/writable-mode.md`).
    fn execute_write(
        &self,
        database_id: &str,
        statement: &Statement,
        started: Instant,
    ) -> Result<QueryOutput, QueryError> {
        let metadata = MetaStore::open(&self.metadata_path)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        if !metadata
            .is_local_database(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?
        {
            // Also the answer for a database that does not exist: a write
            // must never be the thing that reports a missing database as
            // writable.
            return Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            ));
        }
        drop(metadata);

        let outcome =
            pintail_write::LocalDatabase::new(&self.data_dir, &self.metadata_path, database_id)
                .execute(statement)
                .map_err(|error| write_error(&error))?;
        // The catalog and the stored rows both changed; the next read must
        // not answer from a replica loaded before this statement.
        self.cache
            .lock()
            .expect("replica cache lock")
            .remove(database_id);

        let affected = match outcome {
            pintail_write::WriteOutcome::TableCreated { .. } => 0,
            pintail_write::WriteOutcome::RowsInserted { rows, .. } => rows,
        };
        let stats = QueryStats {
            duration_ms: elapsed_ms(started),
            ..QueryStats::default()
        };
        Ok(QueryOutput {
            fields: Vec::new(),
            rows: Vec::new(),
            stats,
            truncated: false,
            affected: Some(affected),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_select(
        &self,
        statement: &Statement,
        catalog: &CatalogSnapshot,
        provider: &SnapshotScanProvider<'_>,
        facts: &SourceFacts,
        database_name: &str,
        table_count: usize,
        started: Instant,
        max_rows: usize,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        let bound = Binder::new(catalog, Some(database_name))
            .bind(statement)
            .map_err(|error| query_bind_error(&error))?;
        let result_nullability = source_result_nullability(&bound, catalog, facts);
        let result_collations = bound
            .projection
            .iter()
            .map(|projection| bound.result_collation(&projection.expr))
            .collect::<Vec<_>>();
        let group_concat = bound
            .projection
            .iter()
            .map(|projection| {
                let pintail_sql::BoundExprKind::Aggregate(slot) = &projection.expr.kind else {
                    return false;
                };
                slot.checked_sub(bound.group_by.len())
                    .and_then(|index| bound.aggregates.get(index))
                    .is_some_and(|aggregate| {
                        aggregate.function == pintail_sql::AggregateFunction::GroupConcat
                    })
            })
            .collect::<Vec<_>>();
        let wire_hints = bound
            .projection
            .iter()
            .map(|projection| {
                let pintail_sql::BoundExprKind::Scalar { function, .. } = &projection.expr.kind
                else {
                    return None;
                };
                match function {
                    pintail_sql::ScalarFunction::SecToTime
                    | pintail_sql::ScalarFunction::MakeTime => Some(WireTypeHint::Time),
                    pintail_sql::ScalarFunction::ConvertTz => Some(WireTypeHint::Datetime),
                    pintail_sql::ScalarFunction::JsonUnquote
                    | pintail_sql::ScalarFunction::JsonExtract { unquote: true } => {
                        Some(WireTypeHint::JsonText)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        // Carried from binding: the binder resolved one collation for this
        // query, and every operator below compares text with it.
        let collation = pintail_exec::collation::Collation::from_mysql_name(bound.text_collation)
            .unwrap_or_default();
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, collation)
            .map_err(|error| QueryError::Invalid(error.to_string()))?;
        let mut execution = Execution::start_with_deadline(
            physical,
            provider,
            self.memory_limit,
            deadline,
            collation,
        )
        .map_err(query_execution_error)?;
        let fields = execution
            .output_fields()
            .iter()
            .enumerate()
            .map(|(index, field)| QueryField {
                name: field.name.clone(),
                data_type: field.data_type,
                nullable: result_nullability
                    .get(index)
                    .copied()
                    .flatten()
                    .unwrap_or(field.nullable),
                collation: result_collations.get(index).cloned().flatten(),
                group_concat: group_concat.get(index).copied().unwrap_or(false),
                geometry: field.geometry,
                timestamp: field.timestamp,
                wire_hint: wire_hints.get(index).copied().flatten(),
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
            affected: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_explain(
        &self,
        statement: &Statement,
        catalog: &CatalogSnapshot,
        provider: &mut SnapshotScanProvider<'_>,
        database_name: &str,
        table_count: usize,
        started: Instant,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        let plan = explain_statement(statement, catalog, Some(database_name)).or_else(|_| {
            explain_analyze_statement_with_deadline(
                statement,
                catalog,
                Some(database_name),
                provider,
                self.memory_limit,
                deadline,
            )
        });
        let plan = plan.map_err(query_explain_error)?;
        let mut stats = provider_stats(provider, table_count);
        stats.duration_ms = elapsed_ms(started);
        stats.rows = 1;
        Ok(QueryOutput {
            fields: vec![QueryField {
                name: "plan".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: false,
                collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
                geometry: false,
                timestamp: false,
                wire_hint: None,
            }],
            rows: vec![vec![Value::Utf8(plan)]],
            stats,
            truncated: false,
            affected: None,
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
    while let Some(batch) = execution.next_batch().map_err(query_execution_error)? {
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

fn query_execution_error(error: ExecError) -> QueryError {
    match error {
        ExecError::QueryTimedOut | ExecError::QueryCancelled => QueryError::Interrupted,
        // MySQL answers a row-wise numeric overflow with 1690/22003, not
        // an internal error - clients branch on the code.
        ExecError::NumericOverflow => QueryError::Rejected {
            rejection: SqlRejection::OutOfRange,
            message: error.to_string(),
        },
        error => QueryError::Internal(error.to_string()),
    }
}

/// Classifies binder rejections into the `MySQL` error classes the wire
/// protocol distinguishes. Anything unclassified keeps today's behaviour
/// (1064 via [`QueryError::Invalid`]).
fn query_bind_error(error: &pintail_sql::BindError) -> QueryError {
    use pintail_sql::BindError;
    let rejection = match &error {
        BindError::UnknownDatabase(_) => SqlRejection::UnknownDatabase,
        BindError::UnknownTable { .. } => SqlRejection::UnknownTable,
        // An unknown relation qualifier surfaces in MySQL as an unknown
        // column ("Unknown column 'u.x' in 'field list'").
        BindError::UnknownColumn(_) | BindError::UnknownRelation(_) => SqlRejection::UnknownColumn,
        BindError::AmbiguousColumn(_)
        | BindError::AmbiguousRelation(_)
        | BindError::AmbiguousOrderBy(_) => SqlRejection::AmbiguousColumn,
        BindError::UngroupedColumn(_) | BindError::UngroupedSubquery => {
            SqlRejection::UngroupedColumn
        }
        BindError::GroupFunctionMisplaced(_) => SqlRejection::GroupFunctionMisplaced,
        _ => return QueryError::Invalid(error.to_string()),
    };
    QueryError::Rejected {
        rejection,
        message: error.to_string(),
    }
}

fn query_explain_error(error: ExplainError) -> QueryError {
    match error {
        ExplainError::Exec(ExecError::QueryTimedOut | ExecError::QueryCancelled) => {
            QueryError::Interrupted
        }
        error => QueryError::Invalid(error.to_string()),
    }
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
                collation: (field.data_type == DataType::Utf8)
                    .then(|| DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
                geometry: false,
                timestamp: false,
                wire_hint: None,
            })
            .collect(),
        stats: QueryStats {
            duration_ms: elapsed_ms(started),
            rows: result.rows.len(),
            ..QueryStats::default()
        },
        rows: result.rows,
        truncated: false,
        affected: None,
    }
}

/// Probe-derived facts the catalog schema does not carry, for
/// `information_schema.columns` fidelity.
fn column_facts(replica: &LoadedReplica) -> SourceFacts {
    let mut facts = SourceFacts::default();
    for target in &replica.targets {
        let source = &target.source;
        for column in &source.columns {
            facts.columns.push(ColumnFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                column: column.name.clone(),
                default_value: column.default_value.clone(),
                default_generated: column.default_generated,
                nullable: Some(column.nullable),
                auto_increment: column.auto_increment,
                generated_stored: column.generated_stored,
                generation_expression: column.generation_expression.clone(),
                extra: column.extra.clone(),
                unique_single: source
                    .unique_keys
                    .iter()
                    .any(|key| key.len() == 1 && key[0].eq_ignore_ascii_case(&column.name)),
                character_set: column.character_set.clone(),
                collation: column.collation.clone(),
                mysql_data_type: Some(column.mysql_data_type.clone()),
                mysql_column_type: Some(column.mysql_column_type.clone()),
            });
        }
        let chosen_unique = matches!(source.key.mode, pintail_types::KeyMode::Unique);
        if chosen_unique {
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                index_name: source
                    .key
                    .index_name
                    .clone()
                    .unwrap_or_else(|| "unique_key".to_owned()),
                unique: true,
                columns: source.key.columns.clone(),
            });
        }
        for key in &source.foreign_keys {
            facts.foreign_keys.push(pintail_sql::ForeignKeyFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                name: key.name.clone(),
                columns: key.columns.clone(),
                referenced_table: key.referenced_table.clone(),
                referenced_columns: key.referenced_columns.clone(),
                unique_constraint_name: key.unique_constraint_name.clone(),
                update_rule: key.update_rule.clone(),
                delete_rule: key.delete_rule.clone(),
            });
        }
        for index in &source.secondary_indexes {
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                index_name: index.name.clone(),
                unique: false,
                columns: index.columns.clone(),
            });
        }
        for (position, unique) in source.unique_keys.iter().enumerate() {
            let is_chosen = chosen_unique
                && unique.len() == source.key.columns.len()
                && unique
                    .iter()
                    .zip(&source.key.columns)
                    .all(|(left, right)| left.eq_ignore_ascii_case(right));
            if is_chosen {
                continue;
            }
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                // The probe keeps unique column sets but not their index
                // names; a synthesized stable name beats hiding the key.
                index_name: format!("unique_{}", position + 1),
                unique: true,
                columns: unique.clone(),
            });
        }
    }
    facts
}

/// Restores source-declared nullability for direct result columns without
/// changing the executor's deliberately permissive physical schema. The
/// physical carrier must allow normalized invalid temporals to become NULL;
/// `MySQL` result metadata still describes a direct source column by its source
/// declaration. Outer-join extension takes precedence over that declaration.
fn collect_null_extended_columns(
    query: &BoundQuery,
    inherited: bool,
    columns: &mut BTreeSet<(DatabaseId, TableId, u32)>,
) {
    for source in &query.from {
        if inherited {
            columns.extend(
                source
                    .base
                    .columns
                    .iter()
                    .map(|column| (column.database_id, column.table_id, column.column_id)),
            );
        }
        if let Some(input) = &source.base.input {
            collect_null_extended_columns(input, inherited, columns);
        }
        for join in &source.joins {
            let right_extended =
                inherited || matches!(join.kind, BoundJoinKind::Left | BoundJoinKind::Scalar);
            if right_extended {
                columns.extend(
                    join.table
                        .columns
                        .iter()
                        .map(|column| (column.database_id, column.table_id, column.column_id)),
                );
            }
            if let Some(input) = &join.table.input {
                collect_null_extended_columns(input, right_extended, columns);
            }
        }
    }
}

fn source_result_nullability(
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Vec<Option<bool>> {
    if query.union_distinct || !query.union_all.is_empty() || !query.set_ops.is_empty() {
        return vec![None; query.projection.len()];
    }
    let mut null_extended = BTreeSet::new();
    collect_null_extended_columns(query, false, &mut null_extended);

    query
        .projection
        .iter()
        .map(|projection| {
            let BoundExprKind::Column(column) = &projection.expr.kind else {
                return None;
            };
            if null_extended.contains(&(column.database_id, column.table_id, column.column_id)) {
                return Some(true);
            }
            let database = catalog.database_by_id(column.database_id)?;
            let table = database.table_by_id(column.table_id)?;
            facts
                .columns
                .iter()
                .find(|fact| {
                    fact.database.eq_ignore_ascii_case(database.name())
                        && fact.table.eq_ignore_ascii_case(table.name())
                        && fact.column.eq_ignore_ascii_case(&column.name)
                })
                .and_then(|fact| fact.nullable)
        })
        .collect()
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
            // rows_synced advances with the snapshot, not with CDC, so it
            // is an estimate: join-size guards may use it, but COUNT(*)
            // must execute (the settled memo and segment SMAs keep that
            // fast) — an exact claim here served stale counts during
            // replication (found by the e2e control-plane gate).
            let entry = TableEntry::new(
                id,
                &target.source.name,
                target.snapshot.schema().clone(),
                TableStatistics::with_estimated_row_count(rows),
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
///
/// Delegates to the single definition in `pintail_store`: readers and
/// writers that disagree here address different directories silently.
#[must_use]
pub fn table_directory(root: &Path, table: &str) -> PathBuf {
    pintail_store::table_directory(root, table)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Maps a write rejection onto the wire's rejection, preserving the `MySQL`
/// error number and SQLSTATE the client branches on.
fn write_error(error: &pintail_write::WriteError) -> QueryError {
    let message = error.to_string();
    let rejection = match error.mysql_code() {
        1050 => SqlRejection::TableExists,
        1062 => SqlRejection::DuplicateKey,
        1048 => SqlRejection::NotNull,
        1146 => SqlRejection::UnknownTable,
        1054 => SqlRejection::UnknownColumn,
        // Everything else is a statement Pintail understood and refused,
        // which is 1064 on the wire like any other unsupported statement.
        _ => return QueryError::Invalid(message),
    };
    QueryError::Rejected { rejection, message }
}
