//! Pintail's HTTP routes and embedded dashboard.

use axum::{
    Json, Router,
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use rust_embed::RustEmbed;
use serde::Serialize;

/// Builds the public HTTP application.
pub fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .route("/{*path}", get(dashboard_asset))
}

#[derive(RustEmbed)]
#[folder = "../../packages/dashboard/.output/public"]
struct Dashboard;

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
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
