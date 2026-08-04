use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use pintail_meta::{ApiKeyRecord, NewApiKey};
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};

use crate::{ApiState, auth::AuthPrincipal, error::ApiError, state::random_identifier};

#[derive(Serialize)]
pub(crate) struct ApiKeyResponse {
    id: String,
    database_id: String,
    name: String,
    enabled: bool,
    scopes: Vec<String>,
    expires_at: Option<String>,
    last_used_at: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
pub(crate) struct CreatedApiKeyResponse {
    #[serde(flatten)]
    key: ApiKeyResponse,
    secret: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateApiKeyRequest {
    name: String,
    #[serde(default = "default_scopes")]
    scopes: Vec<String>,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PatchApiKeyRequest {
    enabled: bool,
}

pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
) -> Result<Json<Vec<ApiKeyResponse>>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    let keys = state
        .metadata()?
        .api_keys(&database_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(ApiKeyResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(keys))
}

pub(crate) async fn create(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(database_id): Path<String>,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<CreatedApiKeyResponse>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("API key name is required"));
    }
    validate_scopes(&request.scopes)?;
    if request
        .expires_at
        .as_deref()
        .is_some_and(expiration_is_invalid)
    {
        return Err(ApiError::bad_request(
            "API key expiration must be a future RFC 3339 timestamp",
        ));
    }
    if state
        .metadata()?
        .database(&database_id)
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found("database does not exist"));
    }
    let id = random_identifier("key_", 16);
    let secret = random_identifier("pk_", 32);
    let digest = Sha256::digest(secret.as_bytes());
    let native_password_hash = Sha1::digest(Sha1::digest(secret.as_bytes()));
    let caching_sha2_hash = Sha256::digest(Sha256::digest(secret.as_bytes()));
    let scopes_json = serde_json::to_string(&request.scopes).map_err(ApiError::internal)?;
    let now = Utc::now().to_rfc3339();
    let metadata = state.metadata()?;
    metadata
        .create_api_key(&NewApiKey {
            id: &id,
            database_id: &database_id,
            name: request.name.trim(),
            sha256: &digest,
            mysql_native_password_hash: Some(&native_password_hash),
            caching_sha2_password_hash: Some(&caching_sha2_hash),
            scopes_json: &scopes_json,
            expires_at: request.expires_at.as_deref(),
            now: &now,
        })
        .map_err(ApiError::internal)?;
    let key = metadata
        .api_keys(&database_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|key| key.id == id)
        .ok_or_else(|| ApiError::internal("created API key disappeared"))?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedApiKeyResponse {
            key: ApiKeyResponse::try_from(key)?,
            secret,
        }),
    ))
}

pub(crate) async fn patch(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, key_id)): Path<(String, String)>,
    Json(request): Json<PatchApiKeyRequest>,
) -> Result<Json<ApiKeyResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    let metadata = state.metadata()?;
    ensure_key_belongs_to(&metadata, &database_id, &key_id)?;
    metadata
        .set_api_key_enabled(&key_id, request.enabled)
        .map_err(ApiError::internal)?;
    let key = metadata
        .api_keys(&database_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|key| key.id == key_id)
        .ok_or_else(|| ApiError::not_found("API key does not exist"))?;
    Ok(Json(ApiKeyResponse::try_from(key)?))
}

pub(crate) async fn delete(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path((database_id, key_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&database_id)?;
    let metadata = state.metadata()?;
    ensure_key_belongs_to(&metadata, &database_id, &key_id)?;
    metadata
        .delete_api_key(&key_id)
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn ensure_key_belongs_to(
    metadata: &pintail_meta::MetaStore,
    database_id: &str,
    key_id: &str,
) -> Result<(), ApiError> {
    if metadata
        .api_keys(database_id)
        .map_err(ApiError::internal)?
        .iter()
        .any(|key| key.id == key_id)
    {
        Ok(())
    } else {
        Err(ApiError::not_found("API key does not exist"))
    }
}

fn validate_scopes(scopes: &[String]) -> Result<(), ApiError> {
    if scopes.is_empty() {
        return Err(ApiError::bad_request(
            "API key must grant at least one scope",
        ));
    }
    for scope in scopes {
        if !matches!(scope.as_str(), "query" | "read") {
            return Err(ApiError::bad_request(format!(
                "unsupported API-key scope {scope}"
            )));
        }
    }
    Ok(())
}

fn default_scopes() -> Vec<String> {
    vec!["query".to_owned(), "read".to_owned()]
}

fn expiration_is_invalid(value: &str) -> bool {
    match DateTime::parse_from_rfc3339(value) {
        Ok(expires) => expires <= Utc::now(),
        Err(_) => true,
    }
}

impl TryFrom<ApiKeyRecord> for ApiKeyResponse {
    type Error = ApiError;

    fn try_from(record: ApiKeyRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: record.id,
            database_id: record.database_id,
            name: record.name,
            enabled: record.enabled,
            scopes: serde_json::from_str(&record.scopes_json).map_err(ApiError::internal)?,
            expires_at: record.expires_at,
            last_used_at: record.last_used_at,
            created_at: record.created_at,
        })
    }
}
