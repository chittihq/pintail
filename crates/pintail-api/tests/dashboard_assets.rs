//! Cache headers on the embedded dashboard.
//!
//! Nuxt code-splits the dashboard into dozens of content-hashed chunks. A
//! captured trace of one page load showed 72 `_nuxt/*` requests carrying
//! only `content-type` and `content-length` - nothing cacheable - so every
//! visit refetched all of them. The bytes are small; the round trips are
//! the cost, and they multiply against a busy origin.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;

async fn header_value(path: &str, name: header::HeaderName) -> Option<String> {
    let response = pintail_api::router()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "for {path}");
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().expect("ascii header").to_owned())
}

#[tokio::test]
async fn hashed_bundles_are_cacheable_forever() {
    // Every `_nuxt/` URL names its own content, so a new build is a new URL
    // and the old one can be held indefinitely.
    let assets = pintail_api::dashboard_asset_paths();
    let hashed = assets
        .iter()
        .find(|path| path.starts_with("_nuxt/"))
        .expect("the dashboard ships hashed bundles");

    let cache_control = header_value(&format!("/{hashed}"), header::CACHE_CONTROL)
        .await
        .unwrap_or_default();
    assert!(
        cache_control.contains("immutable") && cache_control.contains("max-age=31536000"),
        "hashed bundle {hashed} answered cache-control {cache_control:?}"
    );
    assert!(
        header_value(&format!("/{hashed}"), header::ETAG)
            .await
            .is_some(),
        "a hashed bundle needs a validator too"
    );
}

#[tokio::test]
async fn the_page_shell_is_always_revalidated() {
    // The opposite rule: caching the shell would serve the previous
    // release's HTML out of a browser cache after a deploy.
    let cache_control = header_value("/", header::CACHE_CONTROL)
        .await
        .unwrap_or_default();
    assert_eq!(cache_control, "no-cache");
    assert!(header_value("/", header::ETAG).await.is_some());
}
