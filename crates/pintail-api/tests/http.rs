use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn health_endpoint_reports_that_pintail_is_ready() {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("health response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&header::HeaderValue::from_static("application/json"))
    );

    let body = response
        .into_body()
        .collect()
        .await
        .expect("health body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("health JSON");
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn root_serves_the_embedded_dashboard() {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("dashboard request"),
        )
        .await
        .expect("dashboard response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&header::HeaderValue::from_static(
            "text/html; charset=utf-8"
        ))
    );

    let body = response
        .into_body()
        .collect()
        .await
        .expect("dashboard body")
        .to_bytes();
    let html = String::from_utf8(body.to_vec()).expect("dashboard HTML");
    assert!(html.contains("<title>Pintail</title>"));
    assert!(html.contains("Columnar analytics"));
    assert!(html.contains("for MySQL."));
}
