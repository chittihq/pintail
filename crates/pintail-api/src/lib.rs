//! Pintail's authenticated HTTP routes and embedded dashboard.

mod activity;
mod audit;
mod auth;
mod backup;
mod controls;
mod databases;
mod error;
mod events;
mod invites;
mod keys;
mod metrics;
mod oauth;

/// Milliseconds from process start until the API began accepting
/// connections — manifest load, WAL replay and control-plane open. A
/// single-node deployment is unavailable for exactly this long across a
/// restart, so it is the number an operator sizes their tolerance against.
static STARTUP_MILLISECONDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records how long startup took. Called once, after the listeners bind.
pub fn record_startup(elapsed: std::time::Duration) {
    STARTUP_MILLISECONDS.store(
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) fn startup_milliseconds() -> u64 {
    STARTUP_MILLISECONDS.load(std::sync::atomic::Ordering::Relaxed)
}
mod query;
mod snapshot;
mod state;
mod supervisor;
mod workspaces;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::{get, post},
};
use rust_embed::RustEmbed;
use serde::Serialize;

pub use state::ApiState;
pub use supervisor::spawn as spawn_supervisor;

use crate::activity::{activity, dead_letters, discard_dead_letter, retry_dead_letter};
use crate::auth::{login, require_auth, session, setup, setup_status};
use crate::backup::{
    get_config as get_backup_config, list as list_backups, put_config as put_backup_config,
    restore as restore_backup, start as start_backup,
};
use crate::controls::{reconcile, resync};
use crate::databases::{
    create as create_database, delete as delete_database, get as get_database,
    list as list_databases, probe_database, set_mode, status as database_status, test_connection,
    update as update_database,
};
use crate::events::{sse, websocket};
use crate::invites::{create as create_invite, list as list_invites, revoke as revoke_invite};
use crate::keys::{
    create as create_api_key, delete as delete_api_key, list as list_api_keys,
    patch as patch_api_key,
};
use crate::metrics::metrics;
use crate::oauth::{
    callback as google_callback, get_settings as get_google_settings,
    put_settings as put_google_settings, start as google_start, status as google_status,
};
use crate::query::{list_tables, query, table_count, table_data, table_schema};
use crate::snapshot::{start as start_snapshot, status as snapshot_status};
use crate::workspaces::{
    create as create_workspace, list as list_workspaces, members as workspace_members,
    remove_member as remove_workspace_member, switch as switch_workspace,
};

/// Builds the public HTTP application without configured control-plane API
/// state.
///
/// This compatibility constructor serves health and embedded dashboard assets.
/// Use [`router_with_state`] in the Pintail binary.
pub fn router() -> Router {
    router_with_state(ApiState::unconfigured())
}

/// Builds the authenticated HTTP application.
pub fn router_with_state(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/session", get(session))
        .route("/workspaces", get(list_workspaces).post(create_workspace))
        .route("/workspaces/{id}/switch", post(switch_workspace))
        .route("/workspaces/members", get(workspace_members))
        .route(
            "/workspaces/members/{user_id}",
            axum::routing::delete(remove_workspace_member),
        )
        .route(
            "/workspaces/invites",
            get(list_invites).post(create_invite),
        )
        .route(
            "/workspaces/invites/{id}",
            axum::routing::delete(revoke_invite),
        )
        .route(
            "/settings/oauth/google",
            get(get_google_settings).put(put_google_settings),
        )
        .route("/databases", get(list_databases).post(create_database))
        .route(
            "/databases/{id}",
            get(get_database)
                .put(update_database)
                .delete(delete_database),
        )
        .route("/databases/{id}/test", post(test_connection))
        .route("/databases/{id}/probe", get(probe_database))
        .route("/databases/{id}/mode", post(set_mode))
        .route("/databases/{id}/status", get(database_status))
        .route(
            "/databases/{id}/backup-config",
            get(get_backup_config).put(put_backup_config),
        )
        .route(
            "/databases/{id}/backups",
            get(list_backups).post(start_backup),
        )
        .route("/databases/{id}/backups/restore", post(restore_backup))
        .route(
            "/databases/{id}/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route(
            "/databases/{id}/api-keys/{key_id}",
            axum::routing::patch(patch_api_key).delete(delete_api_key),
        )
        .route("/events", get(sse))
        .route("/ws", get(websocket))
        .route("/activity", get(activity))
        .route("/dlq", get(dead_letters))
        .route("/dlq/{id}", axum::routing::delete(discard_dead_letter))
        .route("/dlq/{id}/retry", post(retry_dead_letter))
        .route("/query", post(query))
        .route("/tables", get(list_tables))
        .route("/tables/{name}/schema", get(table_schema))
        .route("/tables/{name}/data", get(table_data))
        .route("/tables/{name}/count", get(table_count))
        .route("/databases/{id}/snapshot", post(start_snapshot))
        .route("/databases/{id}/snapshot/status", get(snapshot_status))
        .route("/databases/{id}/tables/{name}/resync", post(resync))
        .route("/databases/{id}/tables/{name}/reconcile", post(reconcile))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let api = Router::new()
        .route("/auth/setup/status", get(setup_status))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/google/start", get(google_start))
        .route("/auth/google/callback", get(google_callback))
        .route("/auth/google/status", get(google_status))
        .merge(protected);

    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .nest("/api", api)
        .route("/", get(dashboard))
        .route("/{*path}", get(dashboard_asset))
        .with_state(state)
}

#[derive(RustEmbed)]
#[folder = "../../packages/dashboard/.output/public"]
struct Dashboard;

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Serialize)]
struct NodeStatusResponse {
    status: &'static str,
    version: &'static str,
    wire: WireStatus,
}

#[derive(Serialize)]
struct WireStatus {
    enabled: bool,
    bind: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    read_only: bool,
    authentication: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn status(State(state): State<ApiState>) -> Json<NodeStatusResponse> {
    let wire_bind = state.wire_bind();
    Json(NodeStatusResponse {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
        wire: WireStatus {
            enabled: wire_bind.is_some(),
            bind: wire_bind.map(|bind| bind.to_string()),
            host: wire_bind.map(|bind| bind.ip().to_string()),
            port: wire_bind.map(|bind| bind.port()),
            read_only: true,
            authentication: "database_api_key",
        },
    })
}

async fn dashboard() -> Response {
    embedded_asset("index.html")
}

async fn dashboard_asset(Path(path): Path<String>) -> Response {
    // Real asset files (hashed JS/CSS, favicon, fonts) live at their exact
    // embedded path and 404 if truly missing. Route-like paths with no
    // extension are Nuxt pages: nuxt generate prerenders known routes as
    // `{path}/index.html`, but a dynamic route like `/databases/{id}` has
    // no prerendered file, so it falls through to the SPA shell (Nitro's
    // own `200.html`) and Vue Router resolves it client-side.
    if std::path::Path::new(&path).extension().is_some() {
        return embedded_asset(&path);
    }
    let nested_index = format!("{path}/index.html");
    if Dashboard::get(&nested_index).is_some() {
        return embedded_asset(&nested_index);
    }
    embedded_asset("200.html")
}

fn embedded_asset(path: &str) -> Response {
    let Some(asset) = Dashboard::get(path) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .expect("static response is valid");
    };

    let content_type = if std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
    {
        "text/html; charset=utf-8".to_owned()
    } else {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(asset.data.into_owned()))
        .expect("static response is valid")
}
