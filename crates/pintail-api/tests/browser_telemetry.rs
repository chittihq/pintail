use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

#[tokio::test]
async fn browser_configuration_is_public_uncached_and_contains_only_reporting_fields() {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri("/api/telemetry/config")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
    let fields = body.as_object().expect("configuration");
    assert_eq!(fields.len(), 3);
    assert!(fields.contains_key("dsn"));
    assert!(fields["environment"].is_string());
    assert!(fields["release"].is_string());
}
