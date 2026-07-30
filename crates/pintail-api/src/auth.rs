use std::time::Duration;

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher as _, PasswordVerifier as _, Version,
    password_hash::SaltString,
};
use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use jsonwebtoken::{
    Algorithm as JwtAlgorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode,
};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{ApiState, error::ApiError, state::random_identifier};

const TOKEN_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const TOKEN_ISSUER: &str = "pintail";

#[derive(Clone, Debug)]
pub(crate) struct AuthPrincipal {
    pub(crate) subject: String,
    pub(crate) role: String,
    pub(crate) database_id: Option<String>,
    pub(crate) scopes: Vec<String>,
}

impl AuthPrincipal {
    pub(crate) fn require_operator(&self) -> Result<(), ApiError> {
        if matches!(self.role.as_str(), "admin" | "operator") {
            Ok(())
        } else {
            Err(ApiError::forbidden("operator access is required"))
        }
    }

    pub(crate) fn authorize_database(&self, database_id: &str) -> Result<(), ApiError> {
        if self
            .database_id
            .as_deref()
            .is_none_or(|allowed| allowed == database_id)
        {
            Ok(())
        } else {
            Err(ApiError::forbidden("API key is scoped to another database"))
        }
    }

    pub(crate) fn require_scope(&self, scope: &str) -> Result<(), ApiError> {
        if self
            .scopes
            .iter()
            .any(|allowed| allowed == "*" || allowed == scope)
        {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!(
                "authentication does not grant the {scope} scope"
            )))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    role: String,
    iss: String,
    iat: u64,
    exp: u64,
}

#[derive(Serialize)]
pub(crate) struct SetupStatus {
    required: bool,
}

#[derive(Deserialize)]
pub(crate) struct SetupRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
pub(crate) struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Serialize)]
pub(crate) struct SessionResponse {
    token: String,
    user: SessionUser,
}

#[derive(Serialize)]
struct SessionUser {
    id: String,
    email: String,
    role: String,
}

#[derive(Serialize)]
pub(crate) struct PrincipalResponse {
    subject: String,
    role: String,
    database_id: Option<String>,
    scopes: Vec<String>,
}

pub(crate) async fn setup_status(
    State(state): State<ApiState>,
) -> Result<Json<SetupStatus>, ApiError> {
    let metadata = state.metadata()?;
    Ok(Json(SetupStatus {
        required: metadata.user_count().map_err(ApiError::internal)? == 0,
    }))
}

pub(crate) async fn setup(
    State(state): State<ApiState>,
    Json(request): Json<SetupRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    validate_credentials(&request.email, &request.password)?;
    let user_id = random_identifier("usr_", 16);
    let email = request.email.trim().to_ascii_lowercase();
    let password = request.password;
    let metadata_state = state.clone();
    let created_at = Utc::now().to_rfc3339();
    let user_id_for_insert = user_id.clone();
    let email_for_insert = email.clone();
    tokio::task::spawn_blocking(move || {
        let metadata = metadata_state.metadata()?;
        if metadata.user_count().map_err(ApiError::internal)? != 0 {
            return Err(ApiError::conflict("initial admin has already been created"));
        }
        let hash = hash_password(&password)?;
        metadata
            .create_user(
                &user_id_for_insert,
                &email_for_insert,
                &hash,
                "admin",
                &created_at,
            )
            .map_err(ApiError::internal)
    })
    .await
    .map_err(ApiError::internal)??;
    let token = issue_token(&state, &user_id, "admin")?;
    Ok(Json(SessionResponse {
        token,
        user: SessionUser {
            id: user_id,
            email,
            role: "admin".to_owned(),
        },
    }))
}

pub(crate) async fn login(
    State(state): State<ApiState>,
    Json(request): Json<LoginRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let email = request.email.trim().to_ascii_lowercase();
    let password = request.password;
    let metadata_state = state.clone();
    let now = Utc::now().to_rfc3339();
    let user = tokio::task::spawn_blocking(move || {
        let metadata = metadata_state.metadata()?;
        let user = metadata
            .user_by_email(&email)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::unauthorized("email or password is incorrect"))?;
        if !user.enabled || !verify_password(&password, &user.argon2_hash) {
            return Err(ApiError::unauthorized("email or password is incorrect"));
        }
        metadata
            .touch_user_login(&user.id, &now)
            .map_err(ApiError::internal)?;
        Ok(user)
    })
    .await
    .map_err(ApiError::internal)??;
    let token = issue_token(&state, &user.id, &user.role)?;
    Ok(Json(SessionResponse {
        token,
        user: SessionUser {
            id: user.id,
            email: user.email,
            role: user.role,
        },
    }))
}

pub(crate) async fn session(request: Request) -> Result<Json<PrincipalResponse>, ApiError> {
    let principal = request
        .extensions()
        .get::<AuthPrincipal>()
        .ok_or_else(|| ApiError::unauthorized("authentication is required"))?;
    Ok(Json(PrincipalResponse {
        subject: principal.subject.clone(),
        role: principal.role.clone(),
        database_id: principal.database_id.clone(),
        scopes: principal.scopes.clone(),
    }))
}

pub(crate) async fn require_auth(
    State(state): State<ApiState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("Bearer authentication is required"))?;
    let principal = if authorization.starts_with("pk_") {
        authenticate_api_key(&state, authorization)?
    } else {
        authenticate_jwt(&state, authorization)?
    };
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

fn authenticate_jwt(state: &ApiState, token: &str) -> Result<AuthPrincipal, ApiError> {
    let mut validation = Validation::new(JwtAlgorithm::HS256);
    validation.set_issuer(&[TOKEN_ISSUER]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "sub"]);
    let token = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret()?),
        &validation,
    )
    .map_err(|_| ApiError::unauthorized("session token is invalid or expired"))?;
    Ok(AuthPrincipal {
        subject: token.claims.sub,
        role: token.claims.role,
        database_id: None,
        scopes: vec!["*".to_owned()],
    })
}

fn authenticate_api_key(state: &ApiState, secret: &str) -> Result<AuthPrincipal, ApiError> {
    let digest = Sha256::digest(secret.as_bytes());
    let metadata = state.metadata()?;
    let key = metadata
        .api_key_by_sha256(&digest)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("API key is invalid"))?;
    if !key.enabled || key.expires_at.as_deref().is_some_and(is_expired) {
        return Err(ApiError::unauthorized("API key is disabled or expired"));
    }
    metadata
        .touch_api_key(&key.id, &Utc::now().to_rfc3339())
        .map_err(ApiError::internal)?;
    let scopes: Vec<String> = serde_json::from_str(&key.scopes_json).map_err(ApiError::internal)?;
    Ok(AuthPrincipal {
        subject: key.id,
        role: "api_key".to_owned(),
        database_id: Some(key.database_id),
        scopes,
    })
}

fn issue_token(state: &ApiState, subject: &str, role: &str) -> Result<String, ApiError> {
    let issued_at = u64::try_from(Utc::now().timestamp()).map_err(ApiError::internal)?;
    let expires_at = issued_at
        .checked_add(TOKEN_LIFETIME.as_secs())
        .ok_or_else(|| ApiError::internal("JWT expiration overflow"))?;
    let claims = Claims {
        sub: subject.to_owned(),
        role: role.to_owned(),
        iss: TOKEN_ISSUER.to_owned(),
        iat: issued_at,
        exp: expires_at,
    };
    encode(
        &Header::new(JwtAlgorithm::HS256),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret()?),
    )
    .map_err(ApiError::internal)
}

fn hash_password(password: &str) -> Result<String, ApiError> {
    let mut salt_bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(ApiError::internal)?;
    argon2id()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(ApiError::internal)
}

fn verify_password(password: &str, encoded: &str) -> bool {
    PasswordHash::new(encoded).ok().is_some_and(|hash| {
        argon2id()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

fn argon2id() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default())
}

fn validate_credentials(email: &str, password: &str) -> Result<(), ApiError> {
    let email = email.trim();
    if !email.contains('@') || email.len() > 320 {
        return Err(ApiError::bad_request("enter a valid email address"));
    }
    if password.len() < 12 {
        return Err(ApiError::bad_request(
            "password must contain at least 12 characters",
        ));
    }
    Ok(())
}

fn is_expired(value: &str) -> bool {
    match DateTime::parse_from_rfc3339(value) {
        Ok(expires) => expires <= Utc::now(),
        Err(_) => true,
    }
}
