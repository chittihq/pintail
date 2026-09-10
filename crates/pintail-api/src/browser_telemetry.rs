//! Public configuration for error reporting from the embedded dashboard.

use axum::{Json, http::header};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct BrowserTelemetry {
    dsn: Option<String>,
    environment: String,
    release: String,
}

// This endpoint also serves the sign-in screen. Only the public DSN and
// release tags cross the boundary; exporter credentials never do.
pub(crate) async fn config() -> (
    [(header::HeaderName, &'static str); 1],
    Json<BrowserTelemetry>,
) {
    let config = from_values(
        std::env::var("PINTAIL_SENTRY_DSN").ok().as_deref(),
        std::env::var("PINTAIL_ENVIRONMENT").ok().as_deref(),
        std::env::var("PINTAIL_RELEASE").ok().as_deref(),
        std::env::var("PINTAIL_BUILD_VERSION").ok().as_deref(),
    );
    ([(header::CACHE_CONTROL, "no-store")], Json(config))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn from_values(
    dsn: Option<&str>,
    environment: Option<&str>,
    release: Option<&str>,
    build_version: Option<&str>,
) -> BrowserTelemetry {
    BrowserTelemetry {
        dsn: nonempty(dsn).and_then(public_dsn),
        environment: nonempty(environment).unwrap_or("unknown").to_owned(),
        release: nonempty(release)
            .or_else(|| nonempty(build_version))
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_owned(),
    }
}

fn public_dsn(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.username().is_empty() {
        return None;
    }
    let project = url.path().rsplit('/').next()?;
    if project.is_empty() || !project.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // Older DSNs can include a secret key as the URL password. Browser
    // reporting needs only the public key, even when the backend accepted both.
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::{from_values, public_dsn};

    #[test]
    fn browser_dsn_strips_private_fields_and_preserves_proxy_prefix() {
        assert_eq!(
            public_dsn("https://public:private@sentry.example.com/prefix/42?secret=value#fragment"),
            Some("https://public@sentry.example.com/prefix/42".to_owned()),
        );
        assert_eq!(
            public_dsn("http://public@localhost:9000/42"),
            Some("http://public@localhost:9000/42".to_owned()),
        );
    }

    #[test]
    fn invalid_configuration_disables_browser_reporting() {
        for dsn in [
            "",
            "not a url",
            "ftp://public@sentry.example.com/42",
            "https://sentry.example.com/42",
            "https://public@sentry.example.com/",
            "https://public@sentry.example.com/not-a-project",
        ] {
            assert!(from_values(Some(dsn), None, None, None).dsn.is_none());
        }
    }

    #[test]
    fn release_tags_follow_deployment_precedence() {
        let config = from_values(None, Some(" test "), Some("release"), Some("build"));
        assert_eq!(config.release, "release");
        assert_eq!(config.environment, "test");
        assert_eq!(
            from_values(None, None, Some(" "), Some("build")).release,
            "build"
        );
        assert_eq!(
            from_values(None, None, None, None).release,
            env!("CARGO_PKG_VERSION")
        );
    }
}
