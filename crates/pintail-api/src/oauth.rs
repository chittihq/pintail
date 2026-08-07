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
    http::{HeaderMap, HeaderValue, StatusCode, header},
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
const STATE_COOKIE: &str = "pintail_oauth_state";
const STATE_COOKIE_PATH: &str = "/api/auth/google";
const INVITE_LIFETIME_DAYS: i64 = 14;

#[derive(Debug, Clone, Default)]
struct GoogleConfig {
    enabled: bool,
    client_id: String,
    client_secret: String,
    public_origin: String,
}

impl GoogleConfig {
    fn is_active(&self) -> bool {
        self.enabled
            && !self.client_id.is_empty()
            && !self.client_secret.is_empty()
            && !self.public_origin.is_empty()
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
    let public_origin = metadata
        .setting("oauth_google_public_origin")
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
        public_origin,
    })
}

#[derive(Serialize)]
pub(crate) struct GoogleSettingsResponse {
    enabled: bool,
    client_id: String,
    public_url: String,
    configured: bool,
}

#[derive(Deserialize)]
pub(crate) struct PutGoogleSettingsRequest {
    enabled: bool,
    client_id: String,
    #[serde(default)]
    public_url: String,
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
        public_url: config.public_origin,
        configured: !config.client_secret.is_empty(),
    }))
}

pub(crate) async fn put_settings(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<PutGoogleSettingsRequest>,
) -> Result<Json<GoogleSettingsResponse>, ApiError> {
    principal.require_admin()?;
    let public_origin = if request.public_url.trim().is_empty() {
        if request.enabled {
            return Err(ApiError::bad_request(
                "a public URL is required when Google sign-in is enabled",
            ));
        }
        String::new()
    } else {
        normalize_public_origin(&request.public_url)?
    };
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
    metadata
        .set_setting("oauth_google_public_origin", &public_origin)
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
        public_url: config.public_origin,
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

fn sign_state(state: &ApiState) -> Result<(String, String), ApiError> {
    let issued_at = u64::try_from(Utc::now().timestamp()).map_err(ApiError::internal)?;
    let nonce = random_identifier("", 12);
    let claims = StateClaims {
        nonce: nonce.clone(),
        iss: STATE_ISSUER.to_owned(),
        iat: issued_at,
        exp: issued_at + STATE_LIFETIME_SECS,
    };
    let token = encode(
        &Header::new(JwtAlgorithm::HS256),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret()?),
    )
    .map_err(ApiError::internal)?;
    Ok((token, nonce))
}

fn verify_state(state: &ApiState, token: &str, browser_nonce: &str) -> Result<(), ApiError> {
    let mut validation = Validation::new(JwtAlgorithm::HS256);
    validation.set_issuer(&[STATE_ISSUER]);
    validation.set_required_spec_claims(&["exp", "iat", "iss"]);
    let token = decode::<StateClaims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret()?),
        &validation,
    )
    .map_err(|_| ApiError::bad_request("the sign-in attempt expired; try again"))?;
    if token.claims.nonce != browser_nonce {
        return Err(ApiError::bad_request(
            "the sign-in attempt did not originate in this browser",
        ));
    }
    Ok(())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_owned()))
}

fn state_cookie(value: &str, max_age: u64, secure: bool) -> Result<HeaderValue, ApiError> {
    let mut cookie = format!(
        "{STATE_COOKIE}={value}; Path={STATE_COOKIE_PATH}; Max-Age={max_age}; HttpOnly; SameSite=Lax"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).map_err(ApiError::internal)
}

fn normalize_public_origin(value: &str) -> Result<String, ApiError> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| ApiError::bad_request("public URL must be an absolute http(s) URL"))?;
    let is_loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback) {
        return Err(ApiError::bad_request(
            "public URL must use HTTPS except on localhost",
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::bad_request(
            "public URL must contain only scheme, host, and optional port",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

/// The exact URL Google redirects back to. The origin is an administrator
/// setting, never a forwarded header supplied by the callback request.
fn redirect_uri(config: &GoogleConfig) -> String {
    format!("{}/api/auth/google/callback", config.public_origin)
}

pub(crate) async fn start(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let config = load_config(&state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    let callback_uri = redirect_uri(&config);
    let (state_token, browser_nonce) = sign_state(&state)?;
    let mut url = reqwest::Url::parse(AUTH_URL).map_err(ApiError::internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &callback_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &state_token)
        .append_pair("access_type", "online")
        .append_pair("prompt", "select_account");
    let mut response = Redirect::to(url.as_str()).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        state_cookie(
            &browser_nonce,
            STATE_LIFETIME_SECS,
            callback_uri.starts_with("https://"),
        )?,
    );
    Ok(response)
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
    email_verified: bool,
    sub: String,
}

#[derive(Deserialize)]
pub(crate) struct ExchangeRequest {
    code: String,
}

#[derive(Serialize)]
struct ExchangeResponse {
    token: String,
}

pub(crate) async fn exchange(
    State(state): State<ApiState>,
    Json(request): Json<ExchangeRequest>,
) -> Result<Response, ApiError> {
    if request.code.is_empty() {
        return Err(ApiError::bad_request("sign-in exchange code is required"));
    }
    let token = state.consume_oauth_exchange(&request.code)?;
    let mut response = Json(ExchangeResponse { token }).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(crate) async fn callback(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let secure_cookie =
        load_config(&state).is_ok_and(|config| config.public_origin.starts_with("https://"));
    let mut response = match callback_inner(&state, &headers, &query).await {
        Ok(token) => match state.create_oauth_exchange(token) {
            Ok(code) => Redirect::to(&format!("/?auth_code={code}")).into_response(),
            Err(_) => Redirect::to("/?auth_error=sign_in_failed").into_response(),
        },
        Err(error) => {
            let code = match error.status() {
                StatusCode::FORBIDDEN => "not_invited",
                StatusCode::BAD_REQUEST => "invalid_request",
                StatusCode::UNAUTHORIZED => "account_disabled",
                StatusCode::CONFLICT => "link_required",
                _ => "sign_in_failed",
            };
            Redirect::to(&format!("/?auth_error={code}")).into_response()
        }
    };
    if let Ok(cookie) = state_cookie("", 0, secure_cookie) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
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
    let browser_nonce = cookie_value(headers, STATE_COOKIE)
        .ok_or_else(|| ApiError::bad_request("the sign-in browser state is missing"))?;
    verify_state(state, state_token, &browser_nonce)?;

    let config = load_config(state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    let google_user = exchange_code(&config, &redirect_uri(&config), code).await?;
    if !google_user.email_verified {
        return Err(ApiError::bad_request(
            "Google account email is not verified",
        ));
    }
    let email = google_user.email.trim().to_ascii_lowercase();
    let google_subject = google_user.sub.trim();
    if email.is_empty() || google_subject.is_empty() {
        return Err(ApiError::bad_request(
            "Google returned an incomplete account identity",
        ));
    }

    let metadata = state.metadata()?;
    let now = Utc::now().to_rfc3339();

    if let Some(user) = metadata
        .user_by_google_subject(google_subject)
        .map_err(ApiError::internal)?
    {
        if !user.enabled {
            return Err(ApiError::unauthorized("this account is disabled"));
        }
        let (workspace_id, role) = default_workspace_for_user(&metadata, &user.id)?;
        metadata
            .touch_user_login(&user.id, &now)
            .map_err(ApiError::internal)?;
        return issue_token(state, &user.id, &role, &workspace_id);
    }

    if metadata
        .user_by_email(&email)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return Err(ApiError::conflict(
            "an account with this email already exists; sign in with its existing method and link Google explicitly",
        ));
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
        .create_user_via_google(&user_id, &email, google_subject, &invite.role, &now)
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

#[cfg(test)]
mod tests {
    use super::{
        STATE_COOKIE_PATH, normalize_public_origin, sign_state, state_cookie, verify_state,
    };
    use crate::ApiState;

    #[test]
    fn oauth_state_is_bound_to_the_browser_nonce() {
        let data = tempfile::tempdir().expect("temporary API state");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"test-jwt-secret-with-enough-entropy",
            &"11".repeat(32),
        )
        .expect("API state");
        let (token, nonce) = sign_state(&state).expect("signed state");
        verify_state(&state, &token, &nonce).expect("matching browser nonce");
        assert!(verify_state(&state, &token, "another-browser").is_err());
    }

    #[test]
    fn oauth_state_cookie_is_http_only_and_lax() {
        let cookie = state_cookie("nonce", 600, true)
            .expect("state cookie")
            .to_str()
            .expect("ASCII cookie")
            .to_owned();
        assert!(cookie.contains(&format!("Path={STATE_COOKIE_PATH}")));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn oauth_public_origin_is_explicit_and_https() {
        assert_eq!(
            normalize_public_origin("https://pintail.example:8443/").expect("public URL"),
            "https://pintail.example:8443"
        );
        assert_eq!(
            normalize_public_origin("http://localhost:8080").expect("local URL"),
            "http://localhost:8080"
        );
        assert!(normalize_public_origin("http://pintail.example").is_err());
        assert!(normalize_public_origin("https://pintail.example/oauth").is_err());
        assert!(normalize_public_origin("https://user@pintail.example").is_err());
    }
}
