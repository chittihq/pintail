use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use mysql_async::{Opts, Pool, prelude::Queryable as _};
use pintail_meta::{DatabaseRecord, DatabaseUpdate};
use pintail_probe::{RecommendedMode, probe};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::{ApiState, auth::AuthPrincipal, error::ApiError, state::random_identifier};

#[derive(Serialize)]
pub(crate) struct DatabaseResponse {
    id: String,
    name: String,
    mode: String,
    effective_mode: Option<String>,
    state: String,
    include_tables: Vec<String>,
    exclude_tables: Vec<String>,
    poll_interval_seconds: u64,
    reconcile_interval_seconds: u64,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateDatabaseRequest {
    name: String,
    dsn: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    include_tables: Vec<String>,
    #[serde(default)]
    exclude_tables: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct UpdateDatabaseRequest {
    name: String,
    dsn: Option<String>,
    mode: String,
    #[serde(default)]
    include_tables: Vec<String>,
    #[serde(default)]
    exclude_tables: Vec<String>,
    #[serde(default = "default_poll_interval")]
    poll_interval_seconds: u64,
    #[serde(default = "default_reconcile_interval")]
    reconcile_interval_seconds: u64,
}

#[derive(Deserialize)]
pub(crate) struct ModeRequest {
    mode: String,
}

#[derive(Serialize)]
pub(crate) struct TestConnectionResponse {
    ok: bool,
    server_version: String,
}

#[derive(Serialize)]
pub(crate) struct DatabaseStatusResponse {
    database: DatabaseResponse,
    tables: usize,
    rows: u64,
}

pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<Vec<DatabaseResponse>>, ApiError> {
    principal.require_scope("read")?;
    let metadata = state.metadata()?;
    let records = metadata.databases().map_err(ApiError::internal)?;
    let records = records
        .into_iter()
        .filter(|record| {
            principal
                .database_id
                .as_deref()
                .is_none_or(|allowed| allowed == record.id)
        })
        .map(DatabaseResponse::from)
        .collect();
    Ok(Json(records))
}

pub(crate) async fn create(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<CreateDatabaseRequest>,
) -> Result<(StatusCode, Json<DatabaseResponse>), ApiError> {
    principal.require_operator()?;
    validate_database_request(&request.name, &request.dsn, &request.mode)?;
    let id = random_identifier("db_", 16);
    let now = Utc::now().to_rfc3339();
    let encrypted = state.encrypt_dsn(request.dsn.trim())?;
    std::fs::create_dir_all(state.data_dir()?.join("databases").join(&id))
        .map_err(ApiError::internal)?;
    let includes = encode_names(&request.include_tables)?;
    let excludes = encode_names(&request.exclude_tables)?;
    let metadata = state.metadata()?;
    metadata
        .upsert_database(&id, request.name.trim(), &encrypted, &now)
        .map_err(ApiError::internal)?;
    metadata
        .update_database(
            &id,
            &DatabaseUpdate {
                name: request.name.trim(),
                encrypted_dsn: None,
                mode: &request.mode,
                include_tables: Some(&includes),
                exclude_tables: Some(&excludes),
                poll_interval_seconds: default_poll_interval(),
                reconcile_interval_seconds: default_reconcile_interval(),
                now: &now,
            },
        )
        .map_err(ApiError::internal)?;
    let record = metadata
        .database(&id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("created database disappeared"))?;
    Ok((StatusCode::CREATED, Json(DatabaseResponse::from(record))))
}

pub(crate) async fn get(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<DatabaseResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&id)?;
    let record = load_database(&state, &id)?;
    Ok(Json(DatabaseResponse::from(record)))
}

pub(crate) async fn update(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateDatabaseRequest>,
) -> Result<Json<DatabaseResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    let existing = load_database(&state, &id)?;
    let dsn = request.dsn.as_deref().unwrap_or("");
    validate_database_request(
        &request.name,
        if request.dsn.is_some() {
            dsn
        } else {
            "retained"
        },
        &request.mode,
    )?;
    let encrypted = request
        .dsn
        .as_deref()
        .map(|dsn| state.encrypt_dsn(dsn.trim()))
        .transpose()?;
    let includes = encode_names(&request.include_tables)?;
    let excludes = encode_names(&request.exclude_tables)?;
    let now = Utc::now().to_rfc3339();
    let metadata = state.metadata()?;
    metadata
        .update_database(
            &id,
            &DatabaseUpdate {
                name: request.name.trim(),
                encrypted_dsn: encrypted.as_deref(),
                mode: &request.mode,
                include_tables: Some(&includes),
                exclude_tables: Some(&excludes),
                poll_interval_seconds: request.poll_interval_seconds,
                reconcile_interval_seconds: request.reconcile_interval_seconds,
                now: &now,
            },
        )
        .map_err(ApiError::internal)?;
    let mut updated = metadata
        .database(&id)
        .map_err(ApiError::internal)?
        .unwrap_or(existing);
    updated.encrypted_dsn.clear();
    Ok(Json(DatabaseResponse::from(updated)))
}

pub(crate) async fn delete(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    if state
        .metadata()?
        .delete_database(&id)
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("database does not exist"))
    }
}

pub(crate) async fn test_connection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<TestConnectionResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    let record = load_database(&state, &id)?;
    let dsn = state.decrypt_dsn(&record.encrypted_dsn)?;
    let opts = Opts::from_url(&dsn)
        .map_err(|error| ApiError::bad_request(format!("invalid MySQL DSN: {error}")))?;
    let pool = Pool::new(opts);
    let mut connection = pool.get_conn().await.map_err(ApiError::internal)?;
    let version: Option<String> = connection
        .query_first("SELECT @@version")
        .await
        .map_err(ApiError::internal)?;
    drop(connection);
    pool.disconnect().await.map_err(ApiError::internal)?;
    Ok(Json(TestConnectionResponse {
        ok: true,
        server_version: version.unwrap_or_else(|| "unknown".to_owned()),
    }))
}

pub(crate) async fn probe_database(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<JsonValue>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    let record = load_database(&state, &id)?;
    let dsn = state.decrypt_dsn(&record.encrypted_dsn)?;
    let opts = Opts::from_url(&dsn)
        .map_err(|error| ApiError::bad_request(format!("invalid MySQL DSN: {error}")))?;
    let pool = Pool::new(opts);
    let report = probe(&pool, &record.name)
        .await
        .map_err(ApiError::internal)?;
    pool.disconnect().await.map_err(ApiError::internal)?;
    let json = serde_json::to_value(&report).map_err(ApiError::internal)?;
    let encoded = serde_json::to_string(&report).map_err(ApiError::internal)?;
    let effective_mode = match record.mode.as_str() {
        "cdc" => "cdc",
        "polling" => "polling",
        _ => match report.capabilities.recommended_mode {
            RecommendedMode::Cdc => "cdc",
            RecommendedMode::Polling => "polling",
        },
    };
    state
        .metadata()?
        .update_database_probe(&id, &encoded, effective_mode, &Utc::now().to_rfc3339())
        .map_err(ApiError::internal)?;
    Ok(Json(json))
}

pub(crate) async fn set_mode(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<ModeRequest>,
) -> Result<Json<DatabaseResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    state
        .metadata()?
        .set_database_mode(&id, &request.mode, &Utc::now().to_rfc3339())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    get(Extension(principal), State(state), Path(id)).await
}

pub(crate) async fn status(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<DatabaseStatusResponse>, ApiError> {
    principal.require_scope("read")?;
    principal.authorize_database(&id)?;
    let database = load_database(&state, &id)?;
    let tables = state.metadata()?.tables(&id).map_err(ApiError::internal)?;
    let rows = tables.iter().map(|table| table.rows_synced).sum();
    Ok(Json(DatabaseStatusResponse {
        database: DatabaseResponse::from(database),
        tables: tables.len(),
        rows,
    }))
}

fn load_database(state: &ApiState, id: &str) -> Result<DatabaseRecord, ApiError> {
    state
        .metadata()?
        .database(id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("database does not exist"))
}

fn validate_database_request(name: &str, dsn: &str, mode: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() {
        return Err(ApiError::bad_request("database name is required"));
    }
    if dsn.trim().is_empty() {
        return Err(ApiError::bad_request("MySQL DSN is required"));
    }
    if !matches!(mode, "auto" | "cdc" | "polling" | "paused") {
        return Err(ApiError::bad_request(
            "mode must be auto, cdc, polling, or paused",
        ));
    }
    Ok(())
}

fn encode_names(names: &[String]) -> Result<String, ApiError> {
    serde_json::to_string(names).map_err(ApiError::internal)
}

fn decode_names(value: Option<String>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn default_mode() -> String {
    "auto".to_owned()
}

const fn default_poll_interval() -> u64 {
    5
}

const fn default_reconcile_interval() -> u64 {
    600
}

impl From<DatabaseRecord> for DatabaseResponse {
    fn from(record: DatabaseRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            mode: record.mode,
            effective_mode: record.effective_mode,
            state: record.state,
            include_tables: decode_names(record.include_tables),
            exclude_tables: decode_names(record.exclude_tables),
            poll_interval_seconds: record.poll_interval_seconds,
            reconcile_interval_seconds: record.reconcile_interval_seconds,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}
