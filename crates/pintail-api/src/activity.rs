use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use pintail_meta::{DlqRecord, SyncRunRecord};
use serde::{Deserialize, Serialize};

use crate::{ApiState, auth::AuthPrincipal, controls::run_reconcile_job, error::ApiError};

const DEFAULT_LIMIT: u64 = 100;
const MAX_LIMIT: u64 = 1_000;

#[derive(Deserialize)]
pub(crate) struct ActivityQuery {
    db: Option<String>,
    #[serde(default = "default_limit")]
    limit: u64,
}

#[derive(Serialize)]
pub(crate) struct ActivityResponse {
    id: String,
    database_id: String,
    table: Option<String>,
    kind: String,
    status: String,
    rows: u64,
    bytes: u64,
    duration_ms: Option<u64>,
    error: Option<String>,
    started_at: String,
}

#[derive(Serialize)]
pub(crate) struct DlqResponse {
    id: String,
    database_id: String,
    table: Option<String>,
    event: serde_json::Value,
    error: String,
    created_at: String,
}

pub(crate) async fn activity(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<Vec<ActivityResponse>>, ApiError> {
    principal.require_scope("read")?;
    let database = visible_database(&principal, query.db.as_deref())?;
    let runs = state
        .metadata()?
        .sync_runs(database.as_deref(), query.limit.clamp(1, MAX_LIMIT))
        .map_err(ApiError::internal)?
        .into_iter()
        .map(ActivityResponse::from)
        .collect();
    Ok(Json(runs))
}

pub(crate) async fn dead_letters(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<Vec<DlqResponse>>, ApiError> {
    principal.require_scope("read")?;
    let database = visible_database(&principal, query.db.as_deref())?;
    let records = state
        .metadata()?
        .dlq_records(database.as_deref(), query.limit.clamp(1, MAX_LIMIT))
        .map_err(ApiError::internal)?
        .into_iter()
        .map(DlqResponse::from)
        .collect();
    Ok(Json(records))
}

pub(crate) async fn discard_dead_letter(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    principal.require_operator()?;
    if state
        .metadata()?
        .delete_dlq_record(&id)
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("dead-letter record does not exist"))
    }
}

pub(crate) async fn retry_dead_letter(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    principal.require_operator()?;
    let metadata = state.metadata()?;
    let record = metadata
        .dlq_record(&id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("dead-letter record does not exist"))?;
    principal.authorize_database(&record.database_id)?;
    let table = record.table_name.as_deref().ok_or_else(|| {
        ApiError::conflict("database-level dead letters require a database resnapshot")
    })?;
    let table = table.to_owned();
    drop(metadata);
    state.acquire_job(&record.database_id)?;
    let result = run_reconcile_job(&state, &record.database_id, &table).await;
    state.release_job(&record.database_id);
    result.map_err(ApiError::unavailable)?;
    if state
        .metadata()?
        .delete_dlq_record(&id)
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(
            "dead-letter record disappeared after retry",
        ))
    }
}

fn visible_database(
    principal: &AuthPrincipal,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let database = requested
        .map(str::to_owned)
        .or_else(|| principal.database_scope().map(str::to_owned));
    if let Some(database) = &database {
        principal.authorize_database(database)?;
    }
    Ok(database)
}

const fn default_limit() -> u64 {
    DEFAULT_LIMIT
}

impl From<SyncRunRecord> for ActivityResponse {
    fn from(record: SyncRunRecord) -> Self {
        Self {
            id: record.id,
            database_id: record.database_id,
            table: record.table_name,
            kind: record.kind,
            status: record.status,
            rows: record.rows,
            bytes: record.bytes,
            duration_ms: record.duration_ms,
            error: record.error,
            started_at: record.started_at,
        }
    }
}

impl From<DlqRecord> for DlqResponse {
    fn from(record: DlqRecord) -> Self {
        let event = serde_json::from_str(&record.event_json)
            .unwrap_or(serde_json::Value::String(record.event_json));
        Self {
            id: record.id,
            database_id: record.database_id,
            table: record.table_name,
            event,
            error: record.error,
            created_at: record.created_at,
        }
    }
}
