use std::collections::BTreeMap;
use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use pintail_meta::{DatabaseRecord, TableRecord};
use pintail_probe::{ProbeReport, SourceTable};
use pintail_types::{DataType, KeyMode, TableSchema, Value};
use pintail_wire::QueryError;
use serde::{
    Deserialize, Serialize,
    ser::{SerializeSeq, Serializer},
};

use crate::{ApiState, audit, auth::AuthPrincipal, error::ApiError};

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
    collation: Option<String>,
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
    rows: JsonRows,
    stats: QueryStats,
    truncated: bool,
}

/// The engine's own row values, serialized straight to the response writer.
///
/// The previous shape mapped every value through `value_to_json` into a
/// `Vec<Vec<serde_json::Value>>` before axum's `Json` handed it to serde -
/// one full extra tree the same size as the response, allocated and then
/// immediately walked again to write bytes. This wrapper owns the engine's
/// `Vec<Vec<Value>>` unchanged and implements `Serialize` directly against
/// it, so the JSON on the wire is byte-identical (same numbers, strings,
/// `0x`-prefixed binary, `null` for SQL NULL and non-finite floats) without
/// ever materializing the intermediate tree.
struct JsonRows(Vec<Vec<Value>>);

impl JsonRows {
    fn first_cell(&self) -> Option<&Value> {
        self.0.first().and_then(|row| row.first())
    }
}

impl Serialize for JsonRows {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut rows = serializer.serialize_seq(Some(self.0.len()))?;
        for row in &self.0 {
            rows.serialize_element(&JsonRow(row))?;
        }
        rows.end()
    }
}

struct JsonRow<'a>(&'a [Value]);

impl Serialize for JsonRow<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut cells = serializer.serialize_seq(Some(self.0.len()))?;
        for value in self.0 {
            cells.serialize_element(&JsonCell(value))?;
        }
        cells.end()
    }
}

struct JsonCell<'a>(&'a Value);

impl Serialize for JsonCell<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::Boolean(value) => serializer.serialize_bool(*value),
            Value::Int64(value) => serializer.serialize_i64(*value),
            Value::UInt64(value) => serializer.serialize_u64(*value),
            Value::Float64(value) => {
                let value = value.get();
                if value.is_finite() {
                    serializer.serialize_f64(value)
                } else {
                    serializer.serialize_none()
                }
            }
            // JSON callers receive the label, matching the wire surface.
            Value::Utf8(value) | Value::Enum { label: value, .. } => {
                serializer.serialize_str(value)
            }
            Value::DecimalAverage(average) => {
                let value = &average.label;
                serializer.serialize_str(value)
            }
            Value::Binary(value) => serializer.serialize_str(&format!("0x{}", encode_hex(value))),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct TableSummary {
    name: String,
    state: String,
    rows: u64,
    schema_version: u32,
    last_error: Option<String>,
    /// The source has an `ON DELETE/UPDATE CASCADE` or `SET NULL` foreign key
    /// pointing at this table. `MySQL` performs those inside `InnoDB` without
    /// writing row events, so no CDC reader can observe them and the rows are
    /// repaired by scheduled reconciliation instead of arriving in seconds.
    /// Surfaced per row because an operator reading a stale child table needs
    /// to know it is a known mechanism, not a replication fault.
    cascade_reconciled: bool,
    /// Physical identity selected by the source probe. `append_row_id` means
    /// source UPDATE/DELETE events cannot identify one replica row safely.
    key_mode: Option<KeyMode>,
    /// Operator-facing consistency contract for this table in its current
    /// replication mode.
    mutation_guarantee: &'static str,
    /// Required recovery when the normal mutation guarantee cannot apply.
    remediation: Option<&'static str>,
    /// An operator holds the table still: its changes are skipped, not
    /// buffered, until it is resumed.
    paused: bool,
    /// Live copy progress, present only while this table is being copied.
    /// Lets a dashboard that loads mid-copy draw the bar immediately instead
    /// of waiting for the next SSE frame - reloading the page used to reset
    /// the bar to nothing while the copy kept running.
    progress: Option<TableProgressSummary>,
}

#[derive(Serialize)]
pub(crate) struct TableProgressSummary {
    rows: u64,
    eta_seconds: Option<u64>,
    /// How long this copy has been running, so the client can reconstruct
    /// its local start time without trusting clock agreement between the
    /// server and the browser.
    elapsed_seconds: u64,
}

/// Every table's column names in one response, for editor completion.
///
/// The per-table `/tables/{name}/schema` route answers the same question, but a
/// console cannot use it: an 82-table source would need 82 requests before it
/// could complete a single identifier.
///
/// Names only. Types, nullability and key roles are what `/schema` is for, and
/// a completion list that carried them would be several times the size for
/// information the editor does not display.
#[derive(Serialize)]
pub(crate) struct TableColumnsResponse {
    tables: BTreeMap<String, Vec<String>>,
}

/// Serves completion metadata for the whole database.
///
/// Read from the local replica, so it never contacts the source: completion
/// keeps working while the source is unreachable, and typing in the console
/// cannot add load to a production `MySQL`. A table that exists upstream but has
/// not been snapshotted will not appear, which is correct - it cannot be
/// queried here either.
///
/// # Errors
///
/// Returns an error when the database is unknown, unprobed, or its replica
/// cannot be opened.
pub(crate) async fn table_columns(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<TableColumnsResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    crate::databases::load_database(&state, &principal, &query.db)?;
    let replica = load_replica(&state, &query.db)?;
    let tables = replica
        .targets
        .iter()
        .map(|target| {
            let columns = target
                .schema
                .columns()
                .iter()
                .map(|column| column.name().to_owned())
                .collect();
            (target.source.name.clone(), columns)
        })
        .collect();
    Ok(Json(TableColumnsResponse { tables }))
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
    targets: Vec<SchemaTarget>,
}

struct SchemaTarget {
    source: SourceTable,
    schema: TableSchema,
}

pub(crate) async fn query(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    principal.require_scope("query")?;
    principal.authorize_database(&request.db)?;
    crate::databases::load_database(&state, &principal, &request.db)?;
    let response = execute_query(&state, &request.db, &request.sql).await?;
    audit::record(
        &state,
        &principal,
        "query.run",
        Some(("database", &request.db)),
        Some(serde_json::json!({"sql": request.sql, "rows": response.stats.rows})),
    );
    Ok(Json(response))
}

pub(crate) async fn list_tables(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<Vec<TableSummary>>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    crate::databases::load_database(&state, &principal, &query.db)?;
    let database = load_database(&state, &query.db)?;
    let table_facts = probed_table_facts(database.probe_json.as_deref());
    let effective_mode = database.effective_mode.as_deref();
    let tables = state
        .metadata()?
        .tables(&query.db)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|record| {
            let facts = table_facts.get(&record.name.to_ascii_lowercase());
            let key_mode = facts
                .map(|facts| facts.key_mode)
                .or_else(|| durable_key_mode(&record));
            // Stale entries are dropped rather than shown: a copy that has
            // not reported for a minute is dead or wedged, and a frozen bar
            // claiming progress is worse than no bar.
            let progress = state
                .table_progress(&query.db, &record.name)
                .filter(|kept| kept.age_seconds() < 60)
                .map(|kept| TableProgressSummary {
                    rows: kept.rows,
                    eta_seconds: kept.eta_seconds,
                    elapsed_seconds: kept.elapsed_seconds(),
                });
            TableSummary::new(
                record,
                key_mode,
                facts.is_some_and(|facts| facts.cascade_reconciled),
                effective_mode,
                progress,
            )
        })
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
    crate::databases::load_database(&state, &principal, &query.db)?;
    let replica = load_replica(&state, &query.db)?;
    let target = find_target(&replica, &name)?;
    let schema = &target.schema;
    Ok(Json(TableSchemaResponse {
        name: target.source.name.clone(),
        version: schema.version(),
        key_mode: schema.key_mode(),
        key_columns: target.source.key.columns.clone(),
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
    crate::databases::load_database(&state, &principal, &query.db)?;
    let limit = query.limit.clamp(1, MAX_PREVIEW_ROWS);
    let sql = format!(
        "SELECT * FROM `{}` LIMIT {limit} OFFSET {}",
        quote_identifier(&name),
        query.offset
    );
    execute_query(&state, &query.db, &sql).await.map(Json)
}

pub(crate) async fn table_count(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<DatabaseQuery>,
) -> Result<Json<CountResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&query.db)?;
    crate::databases::load_database(&state, &principal, &query.db)?;
    let sql = format!(
        "SELECT COUNT(*) AS `count` FROM `{}`",
        quote_identifier(&name)
    );
    let response = execute_query(&state, &query.db, &sql).await?;
    let count = match response.rows.first_cell() {
        Some(Value::UInt64(count)) => Some(*count),
        Some(Value::Int64(count)) => u64::try_from(*count).ok(),
        _ => None,
    }
    .ok_or_else(|| ApiError::internal("count query did not return an unsigned integer"))?;
    Ok(Json(CountResponse { count }))
}

/// Runs one statement on a blocking thread and shapes its output.
///
/// Blocking, because a query is: it waits for an admission slot, may
/// replay a WAL tail, and then executes for as long as it executes. Run
/// inline it did all of that on the runtime worker that received the
/// request, and with a few dozen HTTP query clients every worker was
/// inside a query - the wire connections and the dashboard's own
/// requests, which need those workers only to move bytes, waited on them.
async fn execute_query(
    state: &ApiState,
    database_id: &str,
    sql: &str,
) -> Result<QueryResponse, ApiError> {
    let debug = std::env::var_os("PINTAIL_API_DEBUG").is_some();
    let started = Instant::now();
    let engine = state
        .replica_engine()?
        .with_memory_limit(state.query_memory_limit());
    let engine_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let (database_id, sql) = (database_id.to_owned(), sql.to_owned());
    let spawn_started = Instant::now();
    let output = tokio::task::spawn_blocking(move || {
        // Nested executions belong to this HTTP query for both victim
        // selection and cancellation, just as they do on the wire path.
        pintail_exec::with_execution_cancellation(
            pintail_exec::ExecutionCancellation::new(),
            || engine.execute(&database_id, &sql, MAX_RESPONSE_ROWS),
        )
    })
    .await
    .map_err(|error| ApiError::internal(format!("query worker failed: {error}")))?
    .map_err(query_error)?;
    let spawn_ms = spawn_started.elapsed().as_secs_f64() * 1_000.0;
    state.record_query(
        output.stats.duration_ms,
        u64::try_from(output.stats.rows).unwrap_or(u64::MAX),
    );
    let serialize_started = Instant::now();
    let response = QueryResponse {
        fields: output
            .fields
            .into_iter()
            .map(|field| QueryField {
                name: field.name,
                data_type: field.data_type,
                nullable: field.nullable,
                collation: field.collation,
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
        rows: JsonRows(output.rows.into_values()),
        truncated: output.truncated,
    };
    // `rows` above only wraps the values; the actual conversion happens
    // later when axum's `Json` extractor serializes the response body, so
    // `serialize_ms` here is everything else in this function (mostly the
    // `fields`/`stats` reshaping) rather than the row cost itself.
    if debug {
        #[allow(clippy::cast_precision_loss)]
        let engine_reported_ms = response.stats.duration_ms as f64;
        eprintln!(
            "[api] query: engine={engine_ms:.2}ms spawn_blocking={spawn_ms:.2}ms \
             (engine reported {engine_reported_ms:.2}ms of it) reshape={:.2}ms total={:.2}ms",
            serialize_started.elapsed().as_secs_f64() * 1_000.0,
            started.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    Ok(response)
}

fn query_error(error: QueryError) -> ApiError {
    match error {
        QueryError::DatabaseNotFound => ApiError::not_found(error.to_string()),
        // Both are "not now, try again": the replica is still catching up,
        // or the engine is at its concurrency bound and never started the
        // query. 503 is what tells a caller or load balancer to retry,
        // which 500 would not.
        QueryError::NotReady(_) | QueryError::Overloaded => {
            ApiError::unavailable(error.to_string())
        }
        QueryError::Invalid(message) => {
            let message = if message == "Pintail's query surfaces are read-only" {
                "Pintail's HTTP query surface is read-only".to_owned()
            } else {
                message
            };
            ApiError::bad_request(message)
        }
        QueryError::Rejected { .. } => ApiError::bad_request(error.to_string()),
        QueryError::Interrupted => ApiError::request_timeout(error.to_string()),
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
    let targets = report
        .tables
        .into_iter()
        .filter(|source| table_records.contains_key(&source.name.to_ascii_lowercase()))
        .map(|mut source| {
            let history = metadata
                .schema_history(database_id, &source.name)
                .map_err(ApiError::internal)?;
            let version = history.last().map_or(1, |record| record.version);
            if let Some(record) = history.last() {
                source.columns =
                    serde_json::from_str(&record.columns_json).map_err(ApiError::internal)?;
            }
            let schema = source
                .table_schema_with_version(version)
                .map_err(ApiError::internal)?;
            Ok(SchemaTarget { source, schema })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LoadedReplica { targets })
}

fn find_target<'replica>(
    replica: &'replica LoadedReplica,
    name: &str,
) -> Result<&'replica SchemaTarget, ApiError> {
    replica
        .targets
        .iter()
        .find(|target| target.source.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| ApiError::not_found("table does not exist"))
}

fn load_database(state: &ApiState, database_id: &str) -> Result<DatabaseRecord, ApiError> {
    state
        .metadata()?
        .database(database_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))
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

impl TableSummary {
    fn new(
        record: TableRecord,
        key_mode: Option<KeyMode>,
        cascade_reconciled: bool,
        effective_mode: Option<&str>,
        progress: Option<TableProgressSummary>,
    ) -> Self {
        let (mutation_guarantee, remediation) =
            match (effective_mode, key_mode, record.state.as_str()) {
                (_, Some(KeyMode::AppendRowId), "needs_resync") => {
                    ("quarantined", Some("resnapshot"))
                }
                (Some("polling"), Some(KeyMode::AppendRowId), _) => {
                    ("generation_replacement", None)
                }
                (_, Some(KeyMode::AppendRowId), _) => {
                    ("insert_only", Some("resnapshot_after_update_or_delete"))
                }
                (Some("polling"), _, _) => ("reconciled_polling", None),
                _ => ("row_level_cdc", None),
            };
        Self {
            name: record.name,
            state: record.state,
            rows: record.rows_synced,
            schema_version: record.schema_version,
            last_error: record.last_error,
            cascade_reconciled,
            key_mode,
            mutation_guarantee,
            remediation,
            paused: record.paused.is_some(),
            progress,
        }
    }
}

fn durable_key_mode(record: &TableRecord) -> Option<KeyMode> {
    if record
        .primary_key_json
        .as_deref()
        .is_none_or(|columns| columns == "[]")
    {
        Some(KeyMode::AppendRowId)
    } else {
        // The durable table row predates explicit key-mode storage: a
        // nonempty key is safe for row-level replication, but cannot tell a
        // PRIMARY key from the selected NOT NULL UNIQUE fallback. Report the
        // classification as unknown rather than silently calling it primary.
        None
    }
}

#[derive(Clone, Copy)]
struct ProbedTableFacts {
    key_mode: KeyMode,
    cascade_reconciled: bool,
}

/// Identity and cascade facts from the last probe. An unreadable or absent
/// report yields none rather than failing the table list: these fields are
/// operator guidance, while corrupt probe state is surfaced by replication.
fn probed_table_facts(probe_json: Option<&str>) -> BTreeMap<String, ProbedTableFacts> {
    probe_json
        .and_then(|json| serde_json::from_str::<pintail_probe::ProbeReport>(json).ok())
        .map(|report| {
            report
                .tables
                .into_iter()
                .map(|table| {
                    (
                        table.name.to_ascii_lowercase(),
                        ProbedTableFacts {
                            key_mode: table.key.mode,
                            cascade_reconciled: table.requires_reconciliation,
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{JsonRows, TableSummary, durable_key_mode, query_error};
    use axum::http::StatusCode;
    use pintail_meta::TableRecord;
    use pintail_types::{KeyMode, Value};
    use pintail_wire::QueryError;

    /// `JsonRows` serializes straight from `Value` without ever building a
    /// `serde_json::Value` tree; this pins its output to the shape the old
    /// tree-building `value_to_json` produced; for one row per variant plus
    /// NULLs and a duplicate, so a shape this covers cannot silently drift.
    #[test]
    fn json_rows_matches_the_tree_building_shape_it_replaced() {
        let rows = JsonRows(vec![
            vec![Value::Null, Value::Boolean(true), Value::Boolean(false)],
            vec![Value::Int64(-7), Value::UInt64(7), Value::UInt64(7)],
            vec![
                Value::float64(1.5),
                Value::float64(f64::NAN),
                Value::float64(f64::INFINITY),
            ],
            vec![
                Value::Utf8("hi".to_owned()),
                Value::Enum {
                    index: 2,
                    label: "b".to_owned(),
                },
            ],
            vec![Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF])],
        ]);
        let json = serde_json::to_value(&rows).expect("rows serialize");
        assert_eq!(
            json,
            serde_json::json!([
                [null, true, false],
                [-7, 7, 7],
                [1.5, null, null],
                ["hi", "b"],
                ["0xdeadbeef"],
            ])
        );
    }

    #[test]
    fn json_rows_first_cell_reads_the_first_row_first_column() {
        assert_eq!(JsonRows(vec![]).first_cell(), None);
        assert_eq!(
            JsonRows(vec![vec![Value::UInt64(42), Value::Null]]).first_cell(),
            Some(&Value::UInt64(42))
        );
    }

    fn table(state: &str, key: Option<&str>) -> TableRecord {
        TableRecord {
            database_id: "db".to_owned(),
            name: "events".to_owned(),
            state: state.to_owned(),
            primary_key_json: key.map(str::to_owned),
            cursor_column: None,
            sort_key_json: key.map(str::to_owned),
            rows_synced: 2,
            last_error: None,
            last_reconcile_at: None,
            schema_version: 1,
            orphaned_at: None,
            soft_delete_column: None,
            copy_complete: true,
            copy_pending: false,
            paused: None,
        }
    }

    #[test]
    fn keyless_table_summary_states_the_safe_mutation_boundary() {
        let insert_only = TableSummary::new(
            table("streaming", Some("[]")),
            Some(KeyMode::AppendRowId),
            false,
            Some("cdc"),
            None,
        );
        assert_eq!(insert_only.mutation_guarantee, "insert_only");
        assert_eq!(
            insert_only.remediation,
            Some("resnapshot_after_update_or_delete")
        );

        let quarantined = TableSummary::new(
            table("needs_resync", Some("[]")),
            Some(KeyMode::AppendRowId),
            false,
            Some("cdc"),
            None,
        );
        assert_eq!(quarantined.mutation_guarantee, "quarantined");
        assert_eq!(quarantined.remediation, Some("resnapshot"));
    }

    #[test]
    fn durable_identity_fallback_distinguishes_empty_keys() {
        assert_eq!(
            durable_key_mode(&table("streaming", Some("[]"))),
            Some(KeyMode::AppendRowId)
        );
        assert_eq!(
            durable_key_mode(&table("streaming", Some("[\"id\"]"))),
            None
        );
    }

    #[test]
    fn interrupted_query_maps_to_request_timeout() {
        assert_eq!(
            query_error(QueryError::Interrupted).status(),
            StatusCode::REQUEST_TIMEOUT
        );
    }
}
