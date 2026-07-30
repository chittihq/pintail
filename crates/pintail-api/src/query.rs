use std::{collections::BTreeMap, time::Instant};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_cdc::CdcTarget;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    explain_analyze_statement, explain_statement,
};
use pintail_meta::{DatabaseRecord, TableRecord};
use pintail_probe::ProbeReport;
use pintail_sql::{Binder, Statement, execute_metadata, parse_statement};
use pintail_store::{StoreOptions, TableSnapshot};
use pintail_types::{DataType, KeyMode, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value as JsonValue};

use crate::{ApiState, auth::AuthPrincipal, error::ApiError, snapshot::table_directory};

const QUERY_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_ROWS: usize = 10_000;
const DEFAULT_PREVIEW_ROWS: usize = 100;
const MAX_PREVIEW_ROWS: usize = 1_000;

#[derive(Deserialize)]
pub(crate) struct QueryRequest {
    db: String,
    sql: String,
}

#[derive(Deserialize)]
pub(crate) struct DatabaseQuery {
    db: String,
}

#[derive(Deserialize)]
pub(crate) struct PreviewQuery {
    db: String,
    #[serde(default = "default_preview_rows")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Clone, Serialize)]
pub(crate) struct QueryField {
    name: String,
    data_type: Option<DataType>,
    nullable: bool,
}

#[derive(Default, Serialize)]
pub(crate) struct QueryStats {
    duration_ms: u64,
    rows: usize,
    batches: usize,
    segments_read: usize,
    segments_pruned: usize,
    blocks_read: usize,
    blocks_pruned: usize,
    blocks_decoded: usize,
}

#[derive(Serialize)]
pub(crate) struct QueryResponse {
    fields: Vec<QueryField>,
    rows: Vec<Vec<JsonValue>>,
    stats: QueryStats,
    truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct TableSummary {
    name: String,
    state: String,
    rows: u64,
    schema_version: u32,
    last_error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct TableSchemaResponse {
    name: String,
    version: u32,
    key_mode: KeyMode,
    key_columns: Vec<String>,
    columns: Vec<TableColumnResponse>,
}

#[derive(Serialize)]
struct TableColumnResponse {
    id: u32,
    name: String,
    data_type: DataType,
    nullable: bool,
}

#[derive(Serialize)]
pub(crate) struct CountResponse {
    count: u64,
}

struct LoadedReplica {
    database: DatabaseRecord,
    tables: Vec<TableRecord>,
    targets: Vec<CdcTarget>,
}

pub(crate) async fn query(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    principal.require_scope("query")?;
    principal.authorize_database(&request.db)?;
    execute_query(&state, &request.db, &request.sql).map(Json)
}

pub(crate) async fn list_tables(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<Vec<TableSummary>>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    load_database(&state, &query.db)?;
    let tables = state
        .metadata()?
        .tables(&query.db)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(TableSummary::from)
        .collect();
    Ok(Json(tables))
}

pub(crate) async fn table_schema(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<TableSchemaResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    let replica = load_replica(&state, &query.db)?;
    let target = find_target(&replica, &name)?;
    let schema = target.store().schema();
    Ok(Json(TableSchemaResponse {
        name: target.source().name.clone(),
        version: schema.version(),
        key_mode: schema.key_mode(),
        key_columns: target.source().key.columns.clone(),
        columns: schema
            .columns()
            .iter()
            .map(|column| TableColumnResponse {
                id: column.id(),
                name: column.name().to_owned(),
                data_type: column.data_type(),
                nullable: column.is_nullable(),
            })
            .collect(),
    }))
}

pub(crate) async fn table_data(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<PreviewQuery>,
) -> Result<Json<QueryResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    let limit = query.limit.clamp(1, MAX_PREVIEW_ROWS);
    let sql = format!(
        "SELECT * FROM `{}` LIMIT {limit} OFFSET {}",
        quote_identifier(&name),
        query.offset
    );
    execute_query(&state, &query.db, &sql).map(Json)
}

pub(crate) async fn table_count(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<CountResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    let sql = format!(
        "SELECT COUNT(*) AS `count` FROM `{}`",
        quote_identifier(&name)
    );
    let response = execute_query(&state, &query.db, &sql)?;
    let count = response
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| ApiError::internal("count query did not return an unsigned integer"))?;
    Ok(Json(CountResponse { count }))
}

fn execute_query(
    state: &ApiState,
    database_id: &str,
    sql: &str,
) -> Result<QueryResponse, ApiError> {
    let started = Instant::now();
    let replica = load_replica(state, database_id)?;
    let snapshots = replica
        .targets
        .iter()
        .map(|target| target.store().snapshot())
        .collect::<Vec<_>>();
    let catalog = build_catalog(&replica, &snapshots)?;
    let mut provider = build_provider(&replica, &snapshots)?;
    let table_count = replica.targets.len();
    let statement =
        parse_statement(sql).map_err(|error| ApiError::bad_request(error.to_string()))?;
    if let Ok(result) = execute_metadata(&statement, &catalog, Some(&replica.database.name)) {
        return Ok(metadata_response(result, started));
    }
    match statement {
        Statement::Query(_) => execute_select(
            &statement,
            &catalog,
            &provider,
            &replica.database.name,
            table_count,
            started,
        ),
        Statement::Explain { .. } => execute_explain(
            &statement,
            &catalog,
            &mut provider,
            &replica.database.name,
            table_count,
            started,
        ),
        _ => Err(ApiError::bad_request(
            "Pintail's HTTP query surface is read-only",
        )),
    }
}

fn execute_select(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    provider: &SnapshotScanProvider<'_>,
    database_name: &str,
    table_count: usize,
    started: Instant,
) -> Result<QueryResponse, ApiError> {
    let bound = Binder::new(catalog, Some(database_name))
        .bind(statement)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical =
        PhysicalPlanner::plan(logical).map_err(|error| ApiError::bad_request(error.to_string()))?;
    let mut execution =
        Execution::start(physical, provider, QUERY_MEMORY_LIMIT).map_err(ApiError::internal)?;
    let fields = execution
        .output_fields()
        .iter()
        .map(|field| QueryField {
            name: field.name.clone(),
            data_type: field.data_type,
            nullable: field.nullable,
        })
        .collect();
    let (rows, batches, truncated) = collect_rows(&mut execution)?;
    let mut stats = provider_stats(provider, table_count);
    stats.duration_ms = elapsed_ms(started);
    stats.rows = rows.len();
    stats.batches = batches;
    Ok(QueryResponse {
        fields,
        rows,
        stats,
        truncated,
    })
}

fn execute_explain(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    provider: &mut SnapshotScanProvider<'_>,
    database_name: &str,
    table_count: usize,
    started: Instant,
) -> Result<QueryResponse, ApiError> {
    let plan = explain_statement(statement, catalog, Some(database_name)).or_else(|_| {
        explain_analyze_statement(
            statement,
            catalog,
            Some(database_name),
            provider,
            QUERY_MEMORY_LIMIT,
        )
    });
    let plan = plan.map_err(|error| ApiError::bad_request(error.to_string()))?;
    let mut stats = provider_stats(provider, table_count);
    stats.duration_ms = elapsed_ms(started);
    stats.rows = 1;
    Ok(QueryResponse {
        fields: vec![QueryField {
            name: "plan".to_owned(),
            data_type: Some(DataType::Utf8),
            nullable: false,
        }],
        rows: vec![vec![JsonValue::String(plan)]],
        stats,
        truncated: false,
    })
}

fn collect_rows(execution: &mut Execution) -> Result<(Vec<Vec<JsonValue>>, usize, bool), ApiError> {
    let mut rows = Vec::new();
    let mut batches = 0;
    while let Some(batch) = execution.next_batch().map_err(ApiError::internal)? {
        batches += 1;
        for row in batch.selection().selected_rows() {
            if rows.len() == MAX_RESPONSE_ROWS {
                return Ok((rows, batches, true));
            }
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column
                        .value(row)
                        .map(value_to_json)
                        .ok_or_else(|| ApiError::internal("query batch has a missing value"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
    }
    Ok((rows, batches, false))
}

fn metadata_response(result: pintail_sql::MetadataResult, started: Instant) -> QueryResponse {
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect::<Vec<_>>();
    QueryResponse {
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
            rows: rows.len(),
            ..QueryStats::default()
        },
        rows,
        truncated: false,
    }
}

fn load_replica(state: &ApiState, database_id: &str) -> Result<LoadedReplica, ApiError> {
    let metadata = state.metadata()?;
    let database = metadata
        .database(database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))?;
    let report: ProbeReport = serde_json::from_str(
        database
            .probe_json
            .as_deref()
            .ok_or_else(|| ApiError::conflict("database has not been probed"))?,
    )
    .map_err(ApiError::internal)?;
    let tables = metadata.tables(database_id).map_err(ApiError::internal)?;
    let table_records = tables
        .iter()
        .map(|table| (table.name.to_ascii_lowercase(), table))
        .collect::<BTreeMap<_, _>>();
    let root = state
        .data_dir()?
        .join("databases")
        .join(database_id)
        .join("tables");
    let targets = report
        .tables
        .into_iter()
        .filter(|source| table_records.contains_key(&source.name.to_ascii_lowercase()))
        .map(|source| {
            let directory = table_directory(&root, &source.name);
            CdcTarget::open_tracked(
                state.metadata_path()?,
                database_id,
                source,
                directory,
                StoreOptions::default(),
            )
            .map_err(|error| {
                ApiError::unavailable(format!(
                    "replica is not currently available for queries: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LoadedReplica {
        database,
        tables,
        targets,
    })
}

fn build_catalog(
    replica: &LoadedReplica,
    snapshots: &[TableSnapshot],
) -> Result<CatalogSnapshot, ApiError> {
    let row_counts = replica
        .tables
        .iter()
        .map(|table| (table.name.to_ascii_lowercase(), table.rows_synced))
        .collect::<BTreeMap<_, _>>();
    let entries = replica
        .targets
        .iter()
        .zip(snapshots)
        .enumerate()
        .map(|(index, (target, snapshot))| {
            let id = table_id(index)?;
            let rows = row_counts
                .get(&target.source().name.to_ascii_lowercase())
                .copied()
                .or(target.source().estimated_rows)
                .unwrap_or(0);
            let entry = TableEntry::new(
                id,
                &target.source().name,
                snapshot.schema().clone(),
                TableStatistics::with_row_count(rows),
            )
            .map_err(ApiError::internal)?;
            let key_columns = target.source().key_column_ids();
            if key_columns.is_empty() {
                Ok(entry)
            } else {
                entry
                    .with_key_columns(key_columns)
                    .map_err(ApiError::internal)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let database = DatabaseEntry::new(DatabaseId::new(1), &replica.database.name, entries)
        .map_err(ApiError::internal)?;
    CatalogSnapshot::new([database]).map_err(ApiError::internal)
}

fn build_provider<'snapshot>(
    replica: &LoadedReplica,
    snapshots: &'snapshot [TableSnapshot],
) -> Result<SnapshotScanProvider<'snapshot>, ApiError> {
    let database_id = DatabaseId::new(1);
    let indexed = snapshots
        .iter()
        .enumerate()
        .map(|(index, snapshot)| Ok((database_id, table_id(index)?, snapshot)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    let mut provider = SnapshotScanProvider::new(indexed).map_err(ApiError::internal)?;
    for (index, target) in replica.targets.iter().enumerate() {
        let unique_keys = target
            .source()
            .unique_keys
            .iter()
            .map(|key| {
                key.iter()
                    .filter_map(|name| {
                        target
                            .source()
                            .columns
                            .iter()
                            .find(|column| column.name.eq_ignore_ascii_case(name))
                            .map(|column| column.id)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|key| !key.is_empty())
            .collect::<Vec<_>>();
        if !unique_keys.is_empty() {
            provider
                .enable_unique_visibility_policy(database_id, table_id(index)?, unique_keys)
                .map_err(ApiError::internal)?;
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

fn find_target<'replica>(
    replica: &'replica LoadedReplica,
    name: &str,
) -> Result<&'replica CdcTarget, ApiError> {
    replica
        .targets
        .iter()
        .find(|target| target.source().name.eq_ignore_ascii_case(name))
        .ok_or_else(|| ApiError::not_found("table does not exist"))
}

fn load_database(state: &ApiState, database_id: &str) -> Result<DatabaseRecord, ApiError> {
    state
        .metadata()?
        .database(database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))
}

fn table_id(index: usize) -> Result<TableId, ApiError> {
    let id = u64::try_from(index)
        .ok()
        .and_then(|index| index.checked_add(1))
        .ok_or_else(|| ApiError::internal("table catalog ID overflow"))?;
    Ok(TableId::new(id))
}

fn value_to_json(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Boolean(value) => JsonValue::Bool(*value),
        Value::Int64(value) => JsonValue::Number(Number::from(*value)),
        Value::UInt64(value) => JsonValue::Number(Number::from(*value)),
        Value::Float64(value) => {
            Number::from_f64(value.get()).map_or(JsonValue::Null, JsonValue::Number)
        }
        Value::Utf8(value) => JsonValue::String(value.clone()),
        Value::Binary(value) => JsonValue::String(format!("0x{}", encode_hex(value))),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn quote_identifier(identifier: &str) -> String {
    identifier.replace('`', "``")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const fn default_preview_rows() -> usize {
    DEFAULT_PREVIEW_ROWS
}

impl From<TableRecord> for TableSummary {
    fn from(record: TableRecord) -> Self {
        Self {
            name: record.name,
            state: record.state,
            rows: record.rows_synced,
            schema_version: record.schema_version,
            last_error: record.last_error,
        }
    }
}
