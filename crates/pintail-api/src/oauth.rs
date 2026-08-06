//! "Sign in with Google" (OAuth 2.0 / OIDC). The client id/secret are
//! configured at runtime from the dashboard (Settings → Sign-in) and stored
//! in the node-global `settings` table — one Google Cloud OAuth client
//! covers every workspace on the node. The client secret is encrypted at
//! rest with the same `ChaCha20Poly1305` key used for `MySQL` DSNs and S3
//! backup credentials, rather than stored in plaintext.
//!
//! Google only ever authenticates an identity that is *already* a member of
//! a workspace, or has a pending invite waiting for its email: there is no
//! open self-signup path through Google.

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use chrono::Utc;
use jsonwebtoken::{
    Algorithm as JwtAlgorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode,
};
use serde::{Deserialize, Serialize};

use crate::{
    ApiState, audit,
    auth::{AuthPrincipal, default_workspace_for_user, issue_token},
    error::ApiError,
    state::random_identifier,
};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const STATE_ISSUER: &str = "pintail-google-oauth-state";
const STATE_LIFETIME_SECS: u64 = 600;
const INVITE_LIFETIME_DAYS: i64 = 14;

#[derive(Debug, Clone, Default)]
struct GoogleConfig {
    enabled: bool,
    client_id: String,
    client_secret: String,
}

impl GoogleConfig {
    fn is_active(&self) -> bool {
        self.enabled && !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
}

fn load_config(state: &ApiState) -> Result<GoogleConfig, ApiError> {
    let metadata = state.metadata()?;
    let enabled = metadata
        .setting("oauth_google_enabled")
        .map_err(ApiError::internal)?
        .as_deref()
        == Some("true");
    let client_id = metadata
        .setting("oauth_google_client_id")
        .map_err(ApiError::internal)?
        .unwrap_or_default();
    let client_secret = match metadata
        .setting("oauth_google_client_secret")
        .map_err(ApiError::internal)?
    {
        Some(encoded) if !encoded.is_empty() => {
            let bytes = decode_hex(&encoded).map_err(ApiError::internal)?;
            state.decrypt_secret(&bytes)?
        }
        _ => String::new(),
    };
    Ok(GoogleConfig {
        enabled,
        client_id,
        client_secret,
    })
}

#[derive(Serialize)]
pub(crate) struct GoogleSettingsResponse {
    enabled: bool,
    client_id: String,
    configured: bool,
}

#[derive(Deserialize)]
pub(crate) struct PutGoogleSettingsRequest {
    enabled: bool,
    client_id: String,
    /// Blank preserves the existing secret, matching the backup-credentials
    /// save form.
    #[serde(default)]
    client_secret: String,
}

pub(crate) async fn get_settings(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<GoogleSettingsResponse>, ApiError> {
    principal.require_admin()?;
    let config = load_config(&state)?;
    Ok(Json(GoogleSettingsResponse {
        enabled: config.enabled,
        client_id: config.client_id,
        configured: !config.client_secret.is_empty(),
    }))
}

pub(crate) async fn put_settings(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<PutGoogleSettingsRequest>,
) -> Result<Json<GoogleSettingsResponse>, ApiError> {
    principal.require_admin()?;
    let metadata = state.metadata()?;
    metadata
        .set_setting(
            "oauth_google_enabled",
            if request.enabled { "true" } else { "false" },
        )
        .map_err(ApiError::internal)?;
    metadata
        .set_setting("oauth_google_client_id", request.client_id.trim())
        .map_err(ApiError::internal)?;
    if !request.client_secret.trim().is_empty() {
        let encrypted = state.encrypt_secret(request.client_secret.trim())?;
        metadata
            .set_setting("oauth_google_client_secret", &encode_hex(&encrypted))
            .map_err(ApiError::internal)?;
    }
    let config = load_config(&state)?;
    audit::record(
        &state,
        &principal,
        "oauth_settings.update",
        None,
        Some(serde_json::json!({"enabled": config.enabled})),
    );
    Ok(Json(GoogleSettingsResponse {
        enabled: config.enabled,
        client_id: config.client_id,
        configured: !config.client_secret.is_empty(),
    }))
}

#[derive(Serialize)]
pub(crate) struct GoogleStatusResponse {
    enabled: bool,
}

/// Public: tells the login page whether to show a "Sign in with Google"
/// button at all.
pub(crate) async fn status(
    State(state): State<ApiState>,
) -> Result<Json<GoogleStatusResponse>, ApiError> {
    let config = load_config(&state)?;
    Ok(Json(GoogleStatusResponse {
        enabled: config.is_active(),
    }))
}

#[derive(Serialize, Deserialize)]
struct StateClaims {
    nonce: String,
    iss: String,
    iat: u64,
    exp: u64,
}

fn sign_state(state: &ApiState) -> Result<String, ApiError> {
    let issued_at = u64::try_from(Utc::now().timestamp()).map_err(ApiError::internal)?;
    let claims = StateClaims {
        nonce: random_identifier("", 12),
        iss: STATE_ISSUER.to_owned(),
        iat: issued_at,
        exp: issued_at + STATE_LIFETIME_SECS,
    };
    encode(
        &Header::new(JwtAlgorithm::HS256),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret()?),
    )
    .map_err(ApiError::internal)
}

fn verify_state(state: &ApiState, token: &str) -> Result<(), ApiError> {
    let mut validation = Validation::new(JwtAlgorithm::HS256);
    validation.set_issuer(&[STATE_ISSUER]);
    validation.set_required_spec_claims(&["exp", "iat", "iss"]);
    decode::<StateClaims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret()?),
        &validation,
    )
    .map(|_| ())
    .map_err(|_| ApiError::bad_request("the sign-in attempt expired; try again"))
}

/// The exact URL Google redirects back to. Must match one of the
/// "Authorized redirect URIs" the admin registered in Google Cloud Console
/// for this OAuth client.
fn redirect_uri(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map_or_else(
            || {
                if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
                    "http".to_owned()
                } else {
                    "https".to_owned()
                }
            },
            str::to_owned,
        );
    format!("{scheme}://{host}/api/auth/google/callback")
}

pub(crate) async fn start(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let config = load_config(&state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    let state_token = sign_state(&state)?;
    let mut url = reqwest::Url::parse(AUTH_URL).map_err(ApiError::internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri(&headers))
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &state_token)
        .append_pair("access_type", "online")
        .append_pair("prompt", "select_account");
    Ok(Redirect::to(url.as_str()).into_response())
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct GoogleUser {
    email: String,
    #[serde(default)]
    email_verified: bool,
    #[serde(default)]
    sub: String,
}

pub(crate) async fn callback(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    match callback_inner(&state, &headers, &query).await {
        Ok(token) => Redirect::to(&format!("/?auth_token={token}")).into_response(),
        Err(error) => {
            let code = match error.status() {
                StatusCode::FORBIDDEN => "not_invited",
                StatusCode::BAD_REQUEST => "invalid_request",
                _ => "sign_in_failed",
            };
            Redirect::to(&format!("/?auth_error={code}")).into_response()
        }
    }
}

async fn callback_inner(
    state: &ApiState,
    headers: &HeaderMap,
    query: &CallbackQuery,
) -> Result<String, ApiError> {
    if let Some(error) = &query.error {
        return Err(ApiError::bad_request(format!(
            "Google sign-in was cancelled: {error}"
        )));
    }
    let code = query
        .code
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("missing authorization code"))?;
    let state_token = query
        .state
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("missing state"))?;
    verify_state(state, state_token)?;

    let config = load_config(state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    let google_user = exchange_code(&config, &redirect_uri(headers), code).await?;
    if !google_user.email_verified {
        return Err(ApiError::bad_request(
            "Google account email is not verified",
        ));
    }
    let email = google_user.email.trim().to_ascii_lowercase();

    let metadata = state.metadata()?;
    let now = Utc::now().to_rfc3339();

    if let Some(user) = metadata
        .user_by_google_subject(&google_user.sub)
        .map_err(ApiError::internal)?
    {
        let (workspace_id, role) = default_workspace_for_user(&metadata, &user.id)?;
        metadata
            .touch_user_login(&user.id, &now)
            .map_err(ApiError::internal)?;
        return issue_token(state, &user.id, &role, &workspace_id);
    }

    if let Some(user) = metadata.user_by_email(&email).map_err(ApiError::internal)? {
        metadata
            .set_user_google_subject(&user.id, &google_user.sub)
            .map_err(ApiError::internal)?;
        let (workspace_id, role) = default_workspace_for_user(&metadata, &user.id)?;
        metadata
            .touch_user_login(&user.id, &now)
            .map_err(ApiError::internal)?;
        return issue_token(state, &user.id, &role, &workspace_id);
    }

    // Brand new identity: only admissible through a pending, unexpired
    // invite for this exact email.
    let invite = metadata
        .invites_by_email(&email)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|invite| {
            invite.accepted_at.is_none()
                && invite.revoked_at.is_none()
                && !is_expired(&invite.expires_at)
        })
        .ok_or_else(|| {
            ApiError::forbidden("this Google account has not been invited to a workspace")
        })?;

    let user_id = random_identifier("usr_", 16);
    metadata
        .create_user_via_google(&user_id, &email, &google_user.sub, &invite.role, &now)
        .map_err(ApiError::internal)?;
    metadata
        .add_workspace_member(&invite.workspace_id, &user_id, &invite.role, &now)
        .map_err(ApiError::internal)?;
    metadata
        .mark_invite_accepted(&invite.id, &now)
        .map_err(ApiError::internal)?;
    let new_member = AuthPrincipal {
        subject: user_id.clone(),
        role: invite.role.clone(),
        database_id: None,
        workspace_id: Some(invite.workspace_id.clone()),
        scopes: vec!["*".to_owned()],
    };
    audit::record(
        state,
        &new_member,
        "invite.accept",
        Some(("invite", &invite.id)),
        Some(serde_json::json!({"email": email})),
    );
    issue_token(state, &user_id, &invite.role, &invite.workspace_id)
}

async fn exchange_code(
    config: &GoogleConfig,
    redirect_uri: &str,
    code: &str,
) -> Result<GoogleUser, ApiError> {
    let client = reqwest::Client::new();
    let token: TokenResponse = client
        .post(TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .map_err(ApiError::internal)?
        .error_for_status()
        .map_err(|error| ApiError::bad_request(format!("Google rejected the sign-in: {error}")))?
        .json()
        .await
        .map_err(ApiError::internal)?;

    client
        .get(USERINFO_URL)
        .bearer_auth(&token.access_token)
        .send()
        .await
        .map_err(ApiError::internal)?
        .error_for_status()
        .map_err(ApiError::internal)?
        .json()
        .await
        .map_err(ApiError::internal)
}

fn is_expired(value: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(expires) => expires <= Utc::now(),
        Err(_) => true,
    }
}

pub(crate) const fn invite_lifetime_days() -> i64 {
    INVITE_LIFETIME_DAYS
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("hex string has odd length".to_owned());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).map_err(|error| error.to_string())
        })
        .collect()
}
