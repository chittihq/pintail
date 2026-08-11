//! Sentry, spoken directly over its envelope endpoint.
//!
//! No SDK. The wire format is three newline-delimited JSON documents and one
//! header, and the SDK's value is in the integrations and context capture this
//! codebase does not use - while its cost is a dependency tree in the process
//! that has to stay running when everything else is failing.

use std::collections::BTreeMap;

use crate::Event;

/// A parsed Sentry DSN.
///
/// `https://<public_key>@<host>/<project_id>`, optionally with a port and a
/// path prefix in front of the project id.
#[derive(Clone, Debug)]
pub(crate) struct Dsn {
    envelope_url: String,
    public_key: String,
}

impl Dsn {
    /// Parses a DSN.
    ///
    /// # Errors
    ///
    /// Returns a description of what was wrong. The DSN itself is never
    /// included: it carries the public key, and a startup error is copied into
    /// issue reports more often than any other line this process prints.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or("a Sentry DSN must start with https:// or http://")?;
        if !matches!(scheme, "https" | "http") {
            return Err("a Sentry DSN must use https or http".to_owned());
        }
        let (public_key, host_and_path) = rest
            .split_once('@')
            .ok_or("a Sentry DSN must contain a public key before @")?;
        // A secret half (`key:secret@`) is legacy and ignored by modern
        // Sentry; accept it and keep only the public key.
        let public_key = public_key.split(':').next().unwrap_or_default();
        if public_key.is_empty() {
            return Err("the Sentry DSN public key is empty".to_owned());
        }
        let (host, path) = host_and_path
            .split_once('/')
            .ok_or("a Sentry DSN must end with /<project_id>")?;
        if host.is_empty() {
            return Err("the Sentry DSN host is empty".to_owned());
        }
        let path = path.trim_matches('/');
        let (prefix, project_id) = match path.rsplit_once('/') {
            Some((prefix, project)) => (format!("/{prefix}"), project),
            None => (String::new(), path),
        };
        if project_id.is_empty() || !project_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("the Sentry DSN project id must be numeric".to_owned());
        }
        Ok(Self {
            envelope_url: format!("{scheme}://{host}{prefix}/api/{project_id}/envelope/"),
            public_key: public_key.to_owned(),
        })
    }

    pub(crate) fn envelope_url(&self) -> &str {
        &self.envelope_url
    }

    /// The `X-Sentry-Auth` header value. Sentry accepts the key in a header or
    /// a query parameter; the header keeps it out of proxy access logs.
    pub(crate) fn auth_header(&self) -> String {
        format!(
            "Sentry sentry_version=7, sentry_client=pintail/{}, sentry_key={}",
            env!("CARGO_PKG_VERSION"),
            self.public_key
        )
    }
}

/// One stack frame, as Sentry wants it.
struct Frame {
    function: String,
    file: Option<String>,
    line: Option<u32>,
}

/// Parses a `std::backtrace::Backtrace` rendering into frames.
///
/// The format is stable enough in practice to be worth parsing: an unparsed
/// backtrace in a message field is a wall of text, while frames give Sentry
/// grouping, a readable stack and the file and line to click through to.
///
/// Anything unrecognised is skipped rather than guessed at. The raw text is
/// attached separately regardless, so a format change degrades to what the
/// text-only version would have given rather than losing the crash.
fn parse_backtrace(rendered: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for line in rendered.lines() {
        let trimmed = line.trim();
        if let Some((index, symbol)) = trimmed.split_once(": ")
            && index.bytes().all(|byte| byte.is_ascii_digit())
            && !index.is_empty()
        {
            frames.push(Frame {
                function: symbol.trim().to_owned(),
                file: None,
                line: None,
            });
        } else if let Some(location) = trimmed.strip_prefix("at ")
            && let Some(frame) = frames.last_mut()
        {
            // `at /path/to/file.rs:120:9` - the column is not used.
            let mut parts = location.rsplitn(3, ':');
            let column_or_line = parts.next().unwrap_or_default();
            let line_or_file = parts.next().unwrap_or_default();
            let remainder = parts.next().unwrap_or_default();
            if remainder.is_empty() {
                frame.file = Some(location.to_owned());
            } else {
                frame.file = Some(remainder.to_owned());
                frame.line = line_or_file
                    .parse()
                    .ok()
                    .or_else(|| column_or_line.parse().ok());
            }
        }
    }
    frames
}

/// Builds the JSON body of a Sentry envelope for one event.
pub(crate) fn envelope(event: &Event, context: &BTreeMap<String, String>) -> String {
    let event_id = event.id.clone();
    let timestamp = event.at.to_rfc3339();
    let level = if event.fatal { "fatal" } else { "error" };

    let mut payload = serde_json::json!({
        "event_id": event_id,
        "timestamp": timestamp,
        "platform": "other",
        "level": level,
        "logger": "pintail",
        "release": context.get("release"),
        "environment": context.get("environment"),
        "server_name": context.get("server_name"),
        "message": { "formatted": event.message },
        "tags": context,
    });

    if let Some(backtrace) = &event.backtrace {
        // Sentry renders frames oldest-first; a Rust backtrace is
        // newest-first, so the order is reversed here rather than in the
        // parser, which stays a faithful reading of the input.
        let frames = parse_backtrace(backtrace)
            .into_iter()
            .rev()
            .map(|frame| {
                serde_json::json!({
                    "function": frame.function,
                    "filename": frame.file,
                    "lineno": frame.line,
                    // Frames inside this workspace are the ones worth showing
                    // expanded; std and backtrace internals are noise.
                    "in_app": frame.file.as_deref().is_some_and(|file| {
                        file.contains("/crates/pintail") || file.starts_with("crates/pintail")
                    }),
                })
            })
            .collect::<Vec<_>>();
        payload["exception"] = serde_json::json!({
            "values": [{
                "type": if event.fatal { "panic" } else { "error" },
                "value": event.message,
                "stacktrace": { "frames": frames },
            }]
        });
        // Kept alongside the parsed frames: if the rendering ever changes
        // shape, this is what makes the report still actionable.
        payload["extra"] = serde_json::json!({ "backtrace": backtrace });
    }

    let header = serde_json::json!({ "event_id": event_id, "sent_at": timestamp });
    let item = serde_json::json!({ "type": "event", "content_type": "application/json" });
    format!("{header}\n{item}\n{payload}\n")
}

#[cfg(test)]
mod tests {
    use super::{Dsn, parse_backtrace};

    #[test]
    fn a_dsn_becomes_an_envelope_url() {
        let dsn = Dsn::parse("https://abc123@o1.ingest.sentry.io/456").expect("valid DSN");
        assert_eq!(
            dsn.envelope_url(),
            "https://o1.ingest.sentry.io/api/456/envelope/"
        );
        assert!(dsn.auth_header().contains("sentry_key=abc123"));
    }

    #[test]
    fn a_path_prefix_is_preserved() {
        let dsn = Dsn::parse("https://key@sentry.example.com/prefix/42").expect("valid DSN");
        assert_eq!(
            dsn.envelope_url(),
            "https://sentry.example.com/prefix/api/42/envelope/"
        );
    }

    #[test]
    fn malformed_dsns_are_refused_without_echoing_them() {
        for raw in [
            "",
            "not-a-url",
            "https://sentry.example.com/42",
            "https://key@sentry.example.com",
            "https://key@sentry.example.com/not-numeric",
            "ftp://key@sentry.example.com/42",
        ] {
            let error = Dsn::parse(raw).expect_err("must be refused");
            assert!(
                !error.contains(raw) || raw.is_empty(),
                "the error echoed the DSN: {error}",
            );
        }
    }

    #[test]
    fn backtrace_frames_carry_their_file_and_line() {
        let rendered = "   0: pintail_cdc::stream::run\n             at ./crates/pintail-cdc/src/lib.rs:412:9\n   1: core::ops::function::FnOnce::call_once\n";
        let frames = parse_backtrace(rendered);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function, "pintail_cdc::stream::run");
        assert_eq!(
            frames[0].file.as_deref(),
            Some("./crates/pintail-cdc/src/lib.rs")
        );
        assert_eq!(frames[0].line, Some(412));
        assert_eq!(frames[1].line, None);
    }
}
