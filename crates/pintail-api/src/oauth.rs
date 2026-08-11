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
use sha2::{Digest as _, Sha256};

use crate::{
    ApiState, audit,
    auth::{AuthPrincipal, default_workspace_for_user, issue_token},
    error::ApiError,
    state::random_identifier,
};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Google's three endpoints, overridable so the browser gate can point the
/// whole flow at a local stand-in.
///
/// Google cannot be driven headlessly, so with these as bare constants the
/// invite → "Continue with Google" path had no test at all - and it is the
/// *only* way a new teammate ever gets an account. Every bug on it therefore
/// reached production first.
///
/// The override is read from the process environment rather than a stored
/// setting deliberately: it is invisible to the dashboard and absent from the
/// metadata database, so no operator - and nobody who reaches the settings
/// API - can redirect sign-in to a server of their choosing. Setting it
/// requires already controlling the process.
fn endpoint(variable: &str, default: &str) -> String {
    std::env::var(variable)
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn auth_endpoint() -> String {
    endpoint("PINTAIL_GOOGLE_AUTH_URL", AUTH_URL)
}

fn token_endpoint() -> String {
    endpoint("PINTAIL_GOOGLE_TOKEN_URL", TOKEN_URL)
}

fn userinfo_endpoint() -> String {
    endpoint("PINTAIL_GOOGLE_USERINFO_URL", USERINFO_URL)
}
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
    intent: StateIntent,
    user_id: Option<String>,
    /// The invite this sign-in is redeeming, resolved from the token on the
    /// link the visitor actually opened.
    ///
    /// Carried through Google in the signed state so the callback claims that
    /// exact invite. Without it admission was resolved by searching every
    /// invite for the address Google returned, which answers a different
    /// question than "which invite did this person accept".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invite_id: Option<String>,
    iss: String,
    iat: u64,
    exp: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StateIntent {
    Login,
    Link,
}

fn sign_state(
    state: &ApiState,
    intent: StateIntent,
    user_id: Option<String>,
    invite_id: Option<String>,
) -> Result<(String, String), ApiError> {
    let issued_at = u64::try_from(Utc::now().timestamp()).map_err(ApiError::internal)?;
    let nonce = random_identifier("", 12);
    let claims = StateClaims {
        nonce: nonce.clone(),
        intent,
        user_id,
        invite_id,
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

fn verify_state(
    state: &ApiState,
    token: &str,
    browser_nonce: &str,
) -> Result<StateClaims, ApiError> {
    let mut validation = Validation::new(JwtAlgorithm::HS256);
    validation.set_issuer(&[STATE_ISSUER]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "intent"]);
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
    Ok(token.claims)
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

fn authorization_url(config: &GoogleConfig, state_token: &str) -> Result<String, ApiError> {
    let mut url = reqwest::Url::parse(&auth_endpoint()).map_err(ApiError::internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri(config))
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", state_token)
        .append_pair("access_type", "online")
        .append_pair("prompt", "select_account");
    Ok(url.into())
}

#[derive(Deserialize)]
pub(crate) struct StartQuery {
    /// The raw invite token from the link the visitor opened, when they
    /// arrived from one.
    #[serde(default)]
    invite: Option<String>,
}

pub(crate) async fn start(
    State(state): State<ApiState>,
    Query(query): Query<StartQuery>,
) -> Result<Response, ApiError> {
    let config = load_config(&state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    // Resolved here, before leaving for Google, so only an invite id travels
    // in the state - never the token itself, which is the bearer credential
    // for the invite and would otherwise pass through Google's URLs and any
    // logs along the way.
    //
    // An unresolvable or unclaimable token is deliberately not an error. The
    // visitor may be an existing member whose link has already been spent, and
    // failing here would refuse a sign-in that is about to succeed on its own
    // merits; the callback refuses anyone who actually needed the invite.
    let invite_id = query
        .invite
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .and_then(|token| {
            let token_hash = Sha256::digest(token.as_bytes());
            state
                .metadata()
                .ok()?
                .invite_by_token_hash(&token_hash)
                .ok()?
                .map(|invite| invite.id)
        });
    let callback_uri = redirect_uri(&config);
    let (state_token, browser_nonce) = sign_state(&state, StateIntent::Login, None, invite_id)?;
    let authorization_url = authorization_url(&config, &state_token)?;
    let mut response = Redirect::to(&authorization_url).into_response();
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

#[derive(Serialize)]
struct LinkStartResponse {
    authorization_url: String,
}

pub(crate) async fn link_start(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Response, ApiError> {
    principal.require_workspace()?;
    let metadata = state.metadata()?;
    let user = metadata
        .user_by_id(&principal.subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("the session user no longer exists"))?;
    if !user.enabled {
        return Err(ApiError::unauthorized("this account is disabled"));
    }
    let config = load_config(&state)?;
    if !config.is_active() {
        return Err(ApiError::conflict("Google sign-in is not configured"));
    }
    let (state_token, browser_nonce) = sign_state(
        &state,
        StateIntent::Link,
        Some(principal.subject.clone()),
        None,
    )?;
    let mut response = Json(LinkStartResponse {
        authorization_url: authorization_url(&config, &state_token)?,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        state_cookie(
            &browser_nonce,
            STATE_LIFETIME_SECS,
            config.public_origin.starts_with("https://"),
        )?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
    outcome: String,
}

pub(crate) async fn exchange(
    State(state): State<ApiState>,
    Json(request): Json<ExchangeRequest>,
) -> Result<Response, ApiError> {
    if request.code.is_empty() {
        return Err(ApiError::bad_request("sign-in exchange code is required"));
    }
    let exchange = state.consume_oauth_exchange(&request.code)?;
    let mut response = Json(ExchangeResponse {
        token: exchange.token,
        outcome: exchange.outcome,
    })
    .into_response();
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
    // Every branch below answers 303, so the access log alone cannot tell a
    // successful sign-in from a refused one: five very different outcomes,
    // one indistinguishable line. The outcome is logged here because the
    // browser is the only other place it appears, and a user reporting "it
    // just spins" cannot be diagnosed from a redirect nobody can see inside.
    //
    // The exchange code is never logged. It is a bearer credential for a
    // session, briefly, and a log is exactly where it must not be.
    let mut response = match callback_inner(&state, &headers, &query).await {
        Ok(success) => match state.create_oauth_exchange(success.token, success.outcome) {
            Ok(code) => {
                pintail_log::log_info!("oauth callback outcome={} redirect=/", success.outcome);
                Redirect::to(&format!("/?auth_code={code}")).into_response()
            }
            Err(error) => {
                // The pending-exchange table is bounded, so this fires when
                // codes are issued and never redeemed - which is what an
                // interrupted sign-in looks like in aggregate.
                pintail_log::log_error!("oauth callback failed to issue an exchange code: {error}");
                Redirect::to("/?auth_error=sign_in_failed").into_response()
            }
        },
        Err(error) => {
            let code = match error.status() {
                StatusCode::FORBIDDEN => "not_invited",
                StatusCode::BAD_REQUEST => "invalid_request",
                StatusCode::UNAUTHORIZED => "account_disabled",
                StatusCode::CONFLICT => "link_required",
                _ => "sign_in_failed",
            };
            // The reason travels with it. "not_invited" names the policy that
            // refused; the message says which check inside it did.
            pintail_log::log_error!("oauth callback rejected={code} reason={error}");
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
) -> Result<CallbackSuccess, ApiError> {
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
    let state_claims = verify_state(state, state_token, &browser_nonce)?;

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

    if state_claims.intent == StateIntent::Link {
        let user_id = state_claims
            .user_id
            .as_deref()
            .ok_or_else(|| ApiError::bad_request("the account-link state is incomplete"))?;
        let token = link_existing_user(state, &metadata, user_id, &email, google_subject, &now)?;
        return Ok(CallbackSuccess {
            token,
            outcome: "linked",
        });
    }

    login_google_user(
        state,
        &metadata,
        &email,
        google_subject,
        &now,
        state_claims.invite_id.as_deref(),
    )
}

struct CallbackSuccess {
    token: String,
    outcome: &'static str,
}

fn is_claimable(invite: &pintail_meta::InviteRecord) -> bool {
    invite.accepted_at.is_none() && invite.revoked_at.is_none() && !is_expired(&invite.expires_at)
}

/// The invite named by the opened link, when it belongs to this address and
/// is still usable.
///
/// A link that no longer resolves is not fatal here. The visitor may be an
/// existing member re-opening a spent link, and refusing would break a
/// sign-in that succeeds on its own merits; whoever actually needed the
/// invite is refused further down. The email must match because the link is a
/// bearer token: without this check, anyone who obtained someone else's invite
/// link could redeem it with their own Google account.
fn redeemable_invite(
    metadata: &pintail_meta::MetaStore,
    invite_id: &str,
    email: &str,
) -> Result<Option<pintail_meta::InviteRecord>, ApiError> {
    let Some(invite) = metadata
        .invite_by_id(invite_id)
        .map_err(ApiError::internal)?
    else {
        pintail_log::log_error!("oauth invite {invite_id} on the opened link no longer exists");
        return Ok(None);
    };
    if !invite.email.eq_ignore_ascii_case(email) {
        pintail_log::log_error!(
            "oauth invite {invite_id} was issued to a different address than the one signing in"
        );
        return Ok(None);
    }
    if !is_claimable(&invite) {
        pintail_log::log_error!("oauth invite {invite_id} is spent, revoked or expired");
        return Ok(None);
    }
    Ok(Some(invite))
}

/// The single claimable invite for an address, for visitors who signed in
/// from the login page rather than an invite link.
///
/// Deliberately refuses when several are open. Selecting the newest across
/// every workspace let an admin of any workspace on the node aim a newer,
/// higher-privileged invite at an address and capture whoever followed a
/// legitimate invite elsewhere - the link opened had no bearing on the
/// workspace or role granted.
fn unambiguous_invite_for(
    metadata: &pintail_meta::MetaStore,
    email: &str,
) -> Result<pintail_meta::InviteRecord, ApiError> {
    let candidates = metadata
        .invites_by_email(email)
        .map_err(ApiError::internal)?;
    let claimable: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, invite)| is_claimable(invite))
        .map(|(position, _)| position)
        .collect();
    match claimable.len() {
        1 => Ok(candidates
            .into_iter()
            .nth(claimable[0])
            .expect("the claimable position was just read from this list")),
        0 => {
            // Which of the four reasons it was matters enormously and is
            // invisible from the browser, where they all render the same.
            if candidates.is_empty() {
                pintail_log::log_error!(
                    "oauth refused {email}: no invite exists for this address; \
                     it may differ from the address the invite was sent to"
                );
            } else {
                let spent = candidates
                    .iter()
                    .map(|invite| {
                        if invite.accepted_at.is_some() {
                            "accepted"
                        } else if invite.revoked_at.is_some() {
                            "revoked"
                        } else {
                            "expired"
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                pintail_log::log_error!(
                    "oauth refused {email}: {} invite(s) exist but none are claimable ({spent})",
                    candidates.len()
                );
            }
            Err(ApiError::forbidden(
                "this Google account has not been invited to a workspace",
            ))
        }
        open => {
            pintail_log::log_error!(
                "oauth refused {email}: {open} invites are open and the sign-in did not name one"
            );
            Err(ApiError::forbidden(
                "several invites are open for this address; open the invite link you were sent so the right one is used",
            ))
        }
    }
}

fn login_google_user(
    state: &ApiState,
    metadata: &pintail_meta::MetaStore,
    email: &str,
    google_subject: &str,
    now: &str,
    invite_id: Option<&str>,
) -> Result<CallbackSuccess, ApiError> {
    // Resolved once, up front, because both branches below need it: the
    // invite the visitor actually opened, when it is theirs and still usable.
    let redeemed = match invite_id {
        Some(id) => redeemable_invite(metadata, id, email)?,
        None => None,
    };

    if let Some(user) = metadata
        .user_by_google_subject(google_subject)
        .map_err(ApiError::internal)?
    {
        if !user.enabled {
            return Err(ApiError::unauthorized("this account is disabled"));
        }
        // An existing identity arriving through an invite redeems it. Without
        // this the branch returned here immediately and the invite was never
        // consulted, which stranded two groups of people: anyone the
        // pre-atomic admission left with a user row and no membership, who was
        // then refused for belonging to no workspace and could not be helped
        // by any number of fresh invites; and any ordinary member invited into
        // a second workspace, whose invite stayed pending forever while they
        // signed into their existing one.
        if let Some(invite) = redeemed {
            metadata
                .admit_existing_user_via_invite(&pintail_meta::GoogleAdmission {
                    user_id: &user.id,
                    email,
                    google_subject,
                    workspace_id: &invite.workspace_id,
                    invite_id: &invite.id,
                    role: &invite.role,
                    now,
                })
                .map_err(ApiError::internal)?;
            metadata
                .touch_user_login(&user.id, now)
                .map_err(ApiError::internal)?;
            let member = AuthPrincipal {
                subject: user.id.clone(),
                role: invite.role.clone(),
                database_id: None,
                workspace_id: Some(invite.workspace_id.clone()),
                scopes: vec!["*".to_owned()],
            };
            audit::record(
                state,
                &member,
                "invite.accept",
                Some(("invite", &invite.id)),
                Some(serde_json::json!({"email": email})),
            );
            return Ok(CallbackSuccess {
                token: issue_token(state, &user.id, &invite.role, &invite.workspace_id)?,
                outcome: "signed_in",
            });
        }
        let (workspace_id, role) = default_workspace_for_user(metadata, &user.id)?;
        metadata
            .touch_user_login(&user.id, now)
            .map_err(ApiError::internal)?;
        return Ok(CallbackSuccess {
            token: issue_token(state, &user.id, &role, &workspace_id)?,
            outcome: "signed_in",
        });
    }

    if metadata
        .user_by_email(email)
        .map_err(ApiError::internal)?
        .is_some()
    {
        pintail_log::log_error!(
            "oauth refused {email}: an account already exists for it without a linked Google identity"
        );
        return Err(ApiError::conflict(
            "an account with this email already exists; sign in with its existing method and link Google explicitly",
        ));
    }

    // Brand new identity: only admissible through an invite. The one the
    // visitor opened wins; the address search is a fallback for people who
    // reached the login page directly, and it refuses rather than guesses.
    let invite = match redeemed {
        Some(invite) => invite,
        None => unambiguous_invite_for(metadata, email)?,
    };

    let user_id = random_identifier("usr_", 16);
    // One transaction. As three separate writes, a failure between creating
    // the user and granting the membership left an account that could never
    // sign in: present enough to skip the invite path, without the workspace
    // that path exists to grant.
    metadata
        .admit_invited_google_user(&pintail_meta::GoogleAdmission {
            user_id: &user_id,
            email,
            google_subject,
            workspace_id: &invite.workspace_id,
            invite_id: &invite.id,
            role: &invite.role,
            now,
        })
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
    Ok(CallbackSuccess {
        token: issue_token(state, &user_id, &invite.role, &invite.workspace_id)?,
        outcome: "signed_in",
    })
}

fn link_existing_user(
    state: &ApiState,
    metadata: &pintail_meta::MetaStore,
    user_id: &str,
    google_email: &str,
    google_subject: &str,
    now: &str,
) -> Result<String, ApiError> {
    let user = metadata
        .user_by_id(user_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("the account no longer exists"))?;
    if !user.enabled {
        return Err(ApiError::unauthorized("this account is disabled"));
    }
    if !user.email.eq_ignore_ascii_case(google_email) {
        return Err(ApiError::conflict(
            "the Google email must match the existing account email",
        ));
    }
    if let Some(owner) = metadata
        .user_by_google_subject(google_subject)
        .map_err(ApiError::internal)?
        && owner.id != user.id
    {
        return Err(ApiError::conflict(
            "this Google account is already linked to another user",
        ));
    }
    if user
        .google_subject
        .as_deref()
        .is_some_and(|subject| subject != google_subject)
    {
        return Err(ApiError::conflict(
            "this account is already linked to a different Google identity",
        ));
    }
    metadata
        .set_user_google_subject(&user.id, google_subject)
        .map_err(ApiError::internal)?;
    let (workspace_id, role) = default_workspace_for_user(metadata, &user.id)?;
    metadata
        .touch_user_login(&user.id, now)
        .map_err(ApiError::internal)?;
    let principal = AuthPrincipal {
        subject: user.id.clone(),
        role: role.clone(),
        database_id: None,
        workspace_id: Some(workspace_id.clone()),
        scopes: vec!["*".to_owned()],
    };
    audit::record(
        state,
        &principal,
        "user.google_link",
        Some(("user", &user.id)),
        Some(serde_json::json!({"email": google_email})),
    );
    issue_token(state, &user.id, &role, &workspace_id)
}

async fn exchange_code(
    config: &GoogleConfig,
    redirect_uri: &str,
    code: &str,
) -> Result<GoogleUser, ApiError> {
    let client = reqwest::Client::new();
    let token: TokenResponse = client
        .post(token_endpoint())
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
        .get(userinfo_endpoint())
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
    if !text.len().is_multiple_of(2) {
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
        STATE_COOKIE_PATH, StateIntent, link_existing_user, normalize_public_origin, sign_state,
        state_cookie, verify_state,
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
        let (token, nonce) =
            sign_state(&state, StateIntent::Login, None, None).expect("signed state");
        let claims = verify_state(&state, &token, &nonce).expect("matching browser nonce");
        assert_eq!(claims.intent, StateIntent::Login);
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

    #[test]
    fn explicit_link_requires_the_same_email_and_never_overwrites() {
        let data = tempfile::tempdir().expect("temporary API state");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"test-jwt-secret-with-enough-entropy",
            &"11".repeat(32),
        )
        .expect("API state");
        let metadata = state.metadata().expect("metadata");
        metadata
            .create_user("usr_test", "member@example.com", "unused", "admin", "now")
            .expect("user");
        metadata
            .create_workspace("ws_test", "Workspace", "workspace", "now")
            .expect("workspace");
        metadata
            .add_workspace_member("ws_test", "usr_test", "admin", "now")
            .expect("membership");

        assert!(
            link_existing_user(
                &state,
                &metadata,
                "usr_test",
                "other@example.com",
                "google-subject",
                "now",
            )
            .is_err()
        );
        link_existing_user(
            &state,
            &metadata,
            "usr_test",
            "member@example.com",
            "google-subject",
            "now",
        )
        .expect("explicit link");
        assert_eq!(
            metadata
                .user_by_id("usr_test")
                .expect("user query")
                .expect("user")
                .google_subject
                .as_deref(),
            Some("google-subject")
        );
        assert!(
            link_existing_user(
                &state,
                &metadata,
                "usr_test",
                "member@example.com",
                "another-subject",
                "now",
            )
            .is_err()
        );
    }
}
