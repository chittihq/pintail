use std::collections::BTreeMap;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use pintail_cdc::CdcTarget;
use pintail_meta::{DatabaseRecord, TableRecord};
use pintail_probe::ProbeReport;
use pintail_store::StoreOptions;
use pintail_types::{DataType, KeyMode, Value};
use pintail_wire::{QueryError, ReplicaEngine, table_directory};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value as JsonValue};

use crate::{ApiState, auth::AuthPrincipal, error::ApiError};

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
    let output = ReplicaEngine::new(state.data_dir()?, state.metadata_path()?)
        .execute(database_id, sql, MAX_RESPONSE_ROWS)
        .map_err(query_error)?;
    state.record_query(
        output.stats.duration_ms,
        u64::try_from(output.stats.rows).unwrap_or(u64::MAX),
    );
    let rows = output
        .rows
        .iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect::<Vec<_>>();
    Ok(QueryResponse {
        fields: output
            .fields
            .into_iter()
            .map(|field| QueryField {
                name: field.name,
                data_type: field.data_type,
                nullable: field.nullable,
            })
            .collect(),
        stats: QueryStats {
            duration_ms: output.stats.duration_ms,
            rows: output.stats.rows,
            batches: output.stats.batches,
            segments_read: output.stats.segments_read,
            segments_pruned: output.stats.segments_pruned,
            blocks_read: output.stats.blocks_read,
            blocks_pruned: output.stats.blocks_pruned,
            blocks_decoded: output.stats.blocks_decoded,
        },
        rows,
        truncated: output.truncated,
    })
}

fn query_error(error: QueryError) -> ApiError {
    match error {
        QueryError::DatabaseNotFound => ApiError::not_found(error.to_string()),
        QueryError::NotReady(_) => ApiError::unavailable(error.to_string()),
        QueryError::Invalid(message) => {
            let message = if message == "Pintail's query surfaces are read-only" {
                "Pintail's HTTP query surface is read-only".to_owned()
            } else {
                message
            };
            ApiError::bad_request(message)
        }
        QueryError::Internal(_) => ApiError::internal(error),
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
    Ok(LoadedReplica { targets })
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
