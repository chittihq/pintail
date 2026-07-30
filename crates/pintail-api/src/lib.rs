//! Pintail's authenticated HTTP routes and embedded dashboard.

mod auth;
mod databases;
mod error;
mod keys;
mod state;

use axum::{
    Json, Router,
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::{get, post},
};
use rust_embed::RustEmbed;
use serde::Serialize;

pub use state::ApiState;

use crate::auth::{login, require_auth, session, setup, setup_status};
use crate::databases::{
    create as create_database, delete as delete_database, get as get_database,
    list as list_databases, probe_database, set_mode, status as database_status, test_connection,
    update as update_database,
};
use crate::keys::{
    create as create_api_key, delete as delete_api_key, list as list_api_keys,
    patch as patch_api_key,
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
            "/databases/{id}/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route(
            "/databases/{id}/api-keys/{key_id}",
            axum::routing::patch(patch_api_key).delete(delete_api_key),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let api = Router::new()
        .route("/auth/setup/status", get(setup_status))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .merge(protected);

    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
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
struct Status {
    status: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn status() -> Json<Status> {
    Json(Status {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn dashboard() -> Response {
    embedded_asset("index.html")
}

async fn dashboard_asset(Path(path): Path<String>) -> Response {
    embedded_asset(&path)
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
