use std::time::{Duration, Instant};

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
/// How long a validated API key stays in the in-process cache before a
/// request re-checks it against metadata. Bounds how long a disabled or
/// expired key can keep authenticating after the change lands, for the
/// (rare) path that reaches this TTL instead of the immediate
/// `invalidate_api_key` call `keys::patch`/`keys::delete` make.
const API_KEY_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(crate) struct AuthPrincipal {
    pub(crate) subject: String,
    pub(crate) role: String,
    pub(crate) database_id: Option<String>,
    /// The workspace this session is scoped to. Present for dashboard-user
    /// (JWT) sessions; absent for database-scoped API keys, which authorize
    /// through [`AuthPrincipal::authorize_database`] instead.
    pub(crate) workspace_id: Option<String>,
    pub(crate) scopes: Vec<String>,
    /// Network peer the request arrived from: the first X-Forwarded-For hop
    /// when a proxy supplied one, otherwise the socket peer. Rides the
    /// principal so every audit record gets it without threading a second
    /// extension through every handler.
    pub(crate) client_ip: Option<String>,
}

impl AuthPrincipal {
    pub(crate) fn database_scope(&self) -> Option<&str> {
        self.database_id.as_deref()
    }

    pub(crate) fn require_operator(&self) -> Result<(), ApiError> {
        if matches!(self.role.as_str(), "admin" | "operator") {
            Ok(())
        } else {
            Err(ApiError::forbidden("operator access is required"))
        }
    }

    pub(crate) fn require_admin(&self) -> Result<(), ApiError> {
        if self.role == "admin" {
            Ok(())
        } else {
            Err(ApiError::forbidden("admin access is required"))
        }
    }

    /// The workspace this dashboard session is scoped to.
    ///
    /// # Errors
    ///
    /// Returns an error when called for an API-key principal, which has no
    /// workspace of its own.
    pub(crate) fn require_workspace(&self) -> Result<&str, ApiError> {
        self.workspace_id
            .as_deref()
            .ok_or_else(|| ApiError::forbidden("a dashboard session is required"))
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

/// Whether this caller administers the node itself, not only a workspace.
///
/// Anyone may create a workspace and is its administrator, so a workspace
/// role says nothing about the node. The node's administrators are the
/// administrators of its first workspace - the one the first-boot setup
/// created (or an upgraded install was given) - and whoever they have made an
/// administrator there. It is read from the membership table on every call,
/// so it follows a demotion or removal immediately, and it holds whichever
/// workspace the session is currently in.
pub(crate) fn is_node_admin(state: &ApiState, principal: &AuthPrincipal) -> Result<bool, ApiError> {
    // An API key is scoped to one database and administers nothing.
    if principal.workspace_id.is_none() {
        return Ok(false);
    }
    let metadata = state.metadata()?;
    let Some(first_workspace) = metadata.first_workspace_id().map_err(ApiError::internal)? else {
        return Ok(false);
    };
    let role = metadata
        .workspace_member_role(&first_workspace, &principal.subject)
        .map_err(ApiError::internal)?;
    Ok(role.as_deref() == Some("admin"))
}

/// The authority a principal holds right now, or `None` when it holds none.
///
/// Authentication answers for the moment a request arrives. Anything that
/// outlives the request - an event stream stays open for hours - has to ask
/// again, or a removed member, a disabled account or a revoked key keeps
/// what it was given. A read that fails answers `None` too: not knowing is
/// no reason to keep serving.
pub(crate) fn current_authority(
    state: &ApiState,
    principal: &AuthPrincipal,
) -> Option<AuthPrincipal> {
    let metadata = state.metadata().ok()?;
    let mut current = principal.clone();
    match (&principal.workspace_id, &principal.database_id) {
        (Some(workspace_id), _) => {
            let user = metadata.user_by_id(&principal.subject).ok()??;
            if !user.enabled {
                return None;
            }
            current.role = metadata
                .workspace_member_role(workspace_id, &principal.subject)
                .ok()??;
        }
        (None, Some(database_id)) => {
            let key = metadata
                .api_keys(database_id)
                .ok()?
                .into_iter()
                .find(|key| key.id == principal.subject)?;
            if !key.enabled || key.expires_at.as_deref().is_some_and(is_expired) {
                return None;
            }
            current.scopes = serde_json::from_str(&key.scopes_json).ok()?;
        }
        (None, None) => return None,
    }
    Some(current)
}

/// Guards a setting that applies to the whole node.
///
/// # Errors
///
/// Returns an error when the caller is not a node administrator.
pub(crate) fn require_node_admin(
    state: &ApiState,
    principal: &AuthPrincipal,
) -> Result<(), ApiError> {
    if is_node_admin(state, principal)? {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "node administrator access is required: this setting applies to every workspace",
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    role: String,
    workspace_id: String,
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
    workspace_id: String,
}

#[derive(Serialize)]
pub(crate) struct PrincipalResponse {
    subject: String,
    role: String,
    database_id: Option<String>,
    workspace_id: Option<String>,
    scopes: Vec<String>,
    /// Whether the caller may change node-wide settings.
    node_admin: bool,
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
    let workspace_id = random_identifier("ws_", 16);
    let email = request.email.trim().to_ascii_lowercase();
    let password = request.password;
    let metadata_state = state.clone();
    let created_at = Utc::now().to_rfc3339();
    let user_id_for_insert = user_id.clone();
    let workspace_id_for_insert = workspace_id.clone();
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
            .map_err(ApiError::internal)?;
        let slug = workspace_id_for_insert.trim_start_matches("ws_");
        metadata
            .create_workspace(&workspace_id_for_insert, "My workspace", slug, &created_at)
            .map_err(ApiError::internal)?;
        metadata
            .add_workspace_member(
                &workspace_id_for_insert,
                &user_id_for_insert,
                "admin",
                &created_at,
            )
            .map_err(ApiError::internal)
    })
    .await
    .map_err(ApiError::internal)??;
    let token = issue_token(&state, &user_id, "admin", &workspace_id)?;
    Ok(Json(SessionResponse {
        token,
        user: SessionUser {
            id: user_id,
            email,
            role: "admin".to_owned(),
            workspace_id,
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
    let (user, workspace_id, role) = tokio::task::spawn_blocking(move || {
        let metadata = metadata_state.metadata()?;
        let user = metadata
            .user_by_email(&email)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::unauthorized("email or password is incorrect"))?;
        if !user.enabled || !verify_password(&password, &user.argon2_hash) {
            return Err(ApiError::unauthorized("email or password is incorrect"));
        }
        let (workspace_id, role) = default_workspace_for_user(&metadata, &user.id)?;
        metadata
            .touch_user_login(&user.id, &now)
            .map_err(ApiError::internal)?;
        Ok((user, workspace_id, role))
    })
    .await
    .map_err(ApiError::internal)??;
    let token = issue_token(&state, &user.id, &role, &workspace_id)?;
    Ok(Json(SessionResponse {
        token,
        user: SessionUser {
            id: user.id,
            email: user.email,
            role,
            workspace_id,
        },
    }))
}

/// Picks the workspace a fresh login lands in: the caller's oldest
/// membership by name order. They can switch afterward from the sidebar.
///
/// # Errors
///
/// Returns an error when the user belongs to no workspace at all.
pub(crate) fn default_workspace_for_user(
    metadata: &pintail_meta::MetaStore,
    user_id: &str,
) -> Result<(String, String), ApiError> {
    let memberships = metadata
        .workspaces_for_user(user_id)
        .map_err(ApiError::internal)?;
    let (workspace, role) = memberships.into_iter().next().ok_or_else(|| {
        // Named, because this is not "you were not invited" - it is "your
        // account exists and belongs to nothing", which reads identically
        // to the user while needing the opposite action from them.
        ApiError::forbidden("this account does not belong to a workspace yet")
            .with_auth_code("no_workspace")
    })?;
    Ok((workspace.id, role))
}

pub(crate) async fn session(
    State(state): State<ApiState>,
    request: Request,
) -> Result<Json<PrincipalResponse>, ApiError> {
    let principal = request
        .extensions()
        .get::<AuthPrincipal>()
        .ok_or_else(|| ApiError::unauthorized("authentication is required"))?;
    Ok(Json(PrincipalResponse {
        node_admin: is_node_admin(&state, principal)?,
        subject: principal.subject.clone(),
        role: principal.role.clone(),
        database_id: principal.database_id.clone(),
        workspace_id: principal.workspace_id.clone(),
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
    let mut principal = if authorization.starts_with("pk_") {
        authenticate_api_key(&state, authorization)?
    } else {
        authenticate_jwt(&state, authorization)?
    };
    principal.client_ip = client_ip_of(&request);
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

/// The network peer a request arrived from, for the audit trail. The first
/// X-Forwarded-For hop wins when a reverse proxy supplied one - the socket
/// peer is the proxy itself in that deployment shape - otherwise the socket
/// peer from the listener's connect info.
fn client_ip_of(request: &Request<Body>) -> Option<String> {
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(forwarded) = forwarded {
        return Some(forwarded.to_owned());
    }
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string())
}

pub(crate) fn authenticate_jwt(state: &ApiState, token: &str) -> Result<AuthPrincipal, ApiError> {
    let mut validation = Validation::new(JwtAlgorithm::HS256);
    validation.set_issuer(&[TOKEN_ISSUER]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "sub"]);
    let token = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret()?),
        &validation,
    )
    .map_err(|_| ApiError::unauthorized("session token is invalid or expired"))?;
    // The claims describe what was true when the token was minted, which can
    // be twelve hours ago. Authorization has to be what is true now.
    //
    // Trusting the embedded role and workspace meant removing a member, or
    // demoting one, changed nothing until their token expired: a removed admin
    // kept admin over the workspace they had been removed from, and could
    // issue fresh admin invites to it, which renews the access indefinitely.
    // Disabling an account had the same delay.
    //
    // The identity in the token is still authenticated by its signature;
    // only the *authority* is re-read: two local reads, on the thread that
    // serves the connection. In the server - the only writer of its store -
    // they are made again only once the store has been written since the
    // last time: an account disabled or a member removed moves the write
    // generation, so the request after it reads the store and is refused,
    // and until then the store would say what it said before.
    let subject = token.claims.sub;
    let workspace_id = token.claims.workspace_id;
    let generation = state.metadata_generation();
    if let Some(generation) = generation
        && let Some(role) = state.standing_role(&subject, &workspace_id, generation)
    {
        return Ok(AuthPrincipal {
            subject,
            role,
            database_id: None,
            workspace_id: Some(workspace_id),
            scopes: vec!["*".to_owned()],
            client_ip: None,
        });
    }
    let metadata = state.metadata()?;
    let user = metadata
        .user_by_id(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("the session user no longer exists"))?;
    if !user.enabled {
        return Err(ApiError::unauthorized("this account is disabled"));
    }
    let role = metadata
        .workspace_member_role(&workspace_id, &subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::unauthorized("this account is no longer a member of that workspace")
        })?;
    if let Some(generation) = generation {
        state.keep_role(&subject, &workspace_id, &role, generation);
    }
    Ok(AuthPrincipal {
        subject,
        role,
        database_id: None,
        workspace_id: Some(workspace_id),
        scopes: vec!["*".to_owned()],
        client_ip: None,
    })
}

pub(crate) fn authenticate_api_key(
    state: &ApiState,
    secret: &str,
) -> Result<AuthPrincipal, ApiError> {
    let started = Instant::now();
    let digest: [u8; 32] = Sha256::digest(secret.as_bytes())
        .as_slice()
        .try_into()
        .expect("SHA-256 digest is 32 bytes");
    let (principal, cache_hit) = match state.cached_api_key(&digest, API_KEY_CACHE_TTL) {
        Some(cached) => (
            AuthPrincipal {
                subject: cached.id,
                role: "api_key".to_owned(),
                database_id: Some(cached.database_id),
                workspace_id: None,
                scopes: cached.scopes,
                client_ip: None,
            },
            true,
        ),
        None => (authenticate_api_key_uncached(state, &digest)?, false),
    };
    if std::env::var_os("PINTAIL_API_DEBUG").is_some() {
        eprintln!(
            "[api] auth: {:.2}ms (cache={})",
            started.elapsed().as_secs_f64() * 1_000.0,
            if cache_hit { "hit" } else { "miss" }
        );
    }
    Ok(principal)
}

/// The cache-miss path: one metadata read validates the key and refreshes
/// `last_used_at`, then the result is cached for `API_KEY_CACHE_TTL` so the
/// next requests on this key skip metadata entirely.
fn authenticate_api_key_uncached(
    state: &ApiState,
    digest: &[u8; 32],
) -> Result<AuthPrincipal, ApiError> {
    let metadata = state.metadata()?;
    let key = metadata
        .api_key_by_sha256(digest)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("API key is invalid"))?;
    if !key.enabled || key.expires_at.as_deref().is_some_and(is_expired) {
        return Err(ApiError::unauthorized("API key is disabled or expired"));
    }
    metadata
        .touch_api_key(&key.id, &Utc::now().to_rfc3339())
        .map_err(ApiError::internal)?;
    let scopes: Vec<String> = serde_json::from_str(&key.scopes_json).map_err(ApiError::internal)?;
    state.cache_api_key(
        *digest,
        key.id.clone(),
        key.database_id.clone(),
        scopes.clone(),
    );
    Ok(AuthPrincipal {
        subject: key.id,
        role: "api_key".to_owned(),
        database_id: Some(key.database_id),
        workspace_id: None,
        scopes,
        client_ip: None,
    })
}

pub(crate) fn issue_token(
    state: &ApiState,
    subject: &str,
    role: &str,
    workspace_id: &str,
) -> Result<String, ApiError> {
    let issued_at = u64::try_from(Utc::now().timestamp()).map_err(ApiError::internal)?;
    let expires_at = issued_at
        .checked_add(TOKEN_LIFETIME.as_secs())
        .ok_or_else(|| ApiError::internal("JWT expiration overflow"))?;
    let claims = Claims {
        sub: subject.to_owned(),
        role: role.to_owned(),
        workspace_id: workspace_id.to_owned(),
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::test_support::Node;

    async fn session(node: &Node, bearer: &str) -> (StatusCode, serde_json::Value) {
        node.call("GET", "/api/session", Some(bearer), None).await
    }

    /// In the server a session's authority is read from the store once and
    /// then stands until the store is written: a journal row leaves it
    /// standing, and a demotion, a removal or a disabled account is seen by
    /// the very next request.
    #[tokio::test]
    async fn a_session_is_read_again_only_after_the_store_is_written() {
        let node = Node::new().await;
        node.state.assume_sole_writer();
        let (member_id, member) = node.member(&node.first_workspace, "viewer");

        // The generation is the process's: a test beside this one that
        // writes its own store moves it too. A round nothing else wrote
        // during is the evidence, and an audit row that moved the
        // generation would leave no such round.
        let mut undisturbed = false;
        for round in 0..64 {
            let generation = pintail_meta::write_generation();
            let (status, first) = session(&node, &member).await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_eq!(first["role"], "viewer");
            node.metadata()
                .record_audit_event(&pintail_meta::NewAuditEvent {
                    id: &format!("audit_example_{round}"),
                    workspace_id: &node.first_workspace,
                    actor_type: "user",
                    actor_id: &member_id,
                    actor_label: "member1@example.com",
                    action: "query.run",
                    target_type: None,
                    target_id: None,
                    detail_json: None,
                    created_at: "2026-10-03T00:00:00Z",
                    client_ip: None,
                })
                .expect("audit");
            let read = node.state.standing_hits();
            let (status, _) = session(&node, &member).await;
            assert_eq!(status, StatusCode::OK);
            if pintail_meta::write_generation() == generation {
                assert_eq!(
                    node.state.standing_hits(),
                    read + 1,
                    "answered without the store, the audit row notwithstanding"
                );
                undisturbed = true;
                break;
            }
        }
        assert!(undisturbed, "an audit row moved the write generation");

        assert!(
            node.metadata()
                .update_workspace_member_role(&node.first_workspace, &member_id, "admin")
                .expect("promotion")
        );
        let read = node.state.standing_hits();
        let (status, promoted) = session(&node, &member).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(promoted["role"], "admin");
        assert_eq!(node.state.standing_hits(), read, "read from the store");

        assert!(
            node.metadata()
                .remove_workspace_member(&node.first_workspace, &member_id)
                .expect("removal")
        );
        let (status, _) = session(&node, &member).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = session(&node, &node.admin).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = session(&node, &node.admin).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            node.metadata()
                .set_user_enabled(&node.admin_id, false)
                .expect("disable")
        );
        let (status, _) = session(&node, &node.admin).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// The database a query names is checked against the store once per
    /// write generation, and a database deleted is gone for the next query.
    #[tokio::test]
    async fn a_queried_database_is_checked_again_only_after_the_store_is_written() {
        let node = Node::new().await;
        node.state.assume_sole_writer();
        let database = node.database(&node.admin, "shop").await;
        let body = format!(r#"{{"db":"{database}","sql":"SELECT 1"}}"#);
        let query = || node.call("POST", "/api/query", Some(&node.admin), Some(&body));

        // As above: one round that nothing else in the process wrote
        // during.
        let mut undisturbed = false;
        for _ in 0..64 {
            let generation = pintail_meta::write_generation();
            let (first, _) = query().await;
            assert_ne!(first, StatusCode::NOT_FOUND);
            let read = node.state.standing_hits();
            let (second, _) = query().await;
            assert_eq!(second, first);
            if pintail_meta::write_generation() == generation {
                assert_eq!(
                    node.state.standing_hits(),
                    read + 2,
                    "the session and the database both stood"
                );
                undisturbed = true;
                break;
            }
        }
        assert!(undisturbed, "a query moved the write generation");

        assert!(node.metadata().delete_database(&database).expect("delete"));
        let (status, _) = query().await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Any other process reads the store on every request, as before.
    #[tokio::test]
    async fn nothing_stands_outside_the_server() {
        let node = Node::new().await;
        for _ in 0..3 {
            let (status, _) = session(&node, &node.admin).await;
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(node.state.standing_hits(), 0);
    }
}
