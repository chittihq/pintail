//! Pintail's authenticated HTTP routes and embedded dashboard.

mod activity;
mod audit;
mod auth;
mod backup;
mod controls;
mod databases;
mod dsn;
mod error;
mod events;
mod invites;
mod keys;
mod metrics;
mod oauth;
mod vitals;
mod wire_certificate;

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
use tower_http::compression::CompressionLayer;

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
    create as create_database, create_local as create_local_database, delete as delete_database,
    get as get_database, list as list_databases, probe_database, set_mode,
    status as database_status, test_connection, update as update_database,
};
use crate::events::{sse, websocket};
use crate::invites::{
    create as create_invite, list as list_invites, revoke as revoke_invite, status as invite_status,
};
use crate::keys::{
    create as create_api_key, delete as delete_api_key, list as list_api_keys,
    patch as patch_api_key,
};
use crate::metrics::metrics;
use crate::oauth::{
    callback as google_callback, exchange as google_exchange, get_settings as get_google_settings,
    link_start as google_link_start, put_settings as put_google_settings, start as google_start,
    status as google_status,
};
use crate::query::{list_tables, query, table_columns, table_count, table_data, table_schema};
use crate::snapshot::{start as start_snapshot, status as snapshot_status};
use crate::workspaces::{
    audit_log as workspace_audit_log, change_member_role as change_workspace_member_role,
    create as create_workspace, list as list_workspaces, members as workspace_members,
    remove_member as remove_workspace_member, switch as switch_workspace,
};

/// Builds the public HTTP application without configured control-plane API
/// state.
///
/// This compatibility constructor serves health and embedded dashboard assets.
/// Use [`router_with_state`] in the Pintail binary.
/// Emits one line per API request: method, path, status, duration.
///
/// Before this the binary logged its two startup lines and nothing else, so a
/// deployment that misbehaved gave an operator nothing to read. A probe that
/// the browser abandoned after its deadline looked exactly like a probe that
/// never ran, and the difference is the whole diagnosis.
///
/// Duration is the point. The capability probe walks every table in the
/// schema, so an 82-table source takes tens of seconds; a line showing
/// `probe 200 14530ms` says the server did its job and the client gave up
/// first, which no amount of client-side logging can establish.
///
/// The query string is deliberately dropped. Invite tokens and the Google
/// one-time exchange code travel there, and an access log is the last place
/// a credential should come to rest.
async fn access_log(request: axum::extract::Request, next: middleware::Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let millis = started.elapsed().as_millis();
    let status = response.status().as_u16();

    // A failing request is logged even at `error` level: it is the line an
    // operator needs most, and suppressing it would leave a 500 invisible.
    let level = if status >= 500 {
        pintail_log::ERROR
    } else {
        pintail_log::INFO
    };
    // Two timings, because they answer different questions and this
    // investigation turned on the gap between them: `handled` is how long
    // the handler took to PRODUCE the response, `sent` is how long until
    // its last byte reached the client. A 3ms handler whose body takes 40
    // seconds to deliver looks perfectly healthy in a handler-only log,
    // which is exactly how a real slowdown stayed invisible here.
    let line = format!("{method} {path} {status} handled={millis}ms");
    if !pintail_log::enabled(level) {
        return response;
    }
    let (parts, body) = response.into_parts();
    let expected = http_body::Body::size_hint(&body).exact();
    Response::from_parts(
        parts,
        Body::new(TimedBody {
            inner: body,
            started,
            line,
            expected,
            delivered: 0,
            reported: false,
        }),
    )
}

/// A response body that reports total delivery time when its last frame is
/// read, or when it is dropped early because the client went away.
struct TimedBody {
    inner: Body,
    started: std::time::Instant,
    line: String,
    /// Body length when it is known up front, to tell a completed transfer
    /// from one the client gave up on.
    expected: Option<u64>,
    delivered: u64,
    reported: bool,
}

impl TimedBody {
    fn report(&mut self) {
        if self.reported {
            return;
        }
        self.reported = true;
        // Hyper does not always poll a fixed-length body to its end - it can
        // take the whole thing at once - so completion is decided by bytes
        // delivered against bytes promised, not by reaching the final frame.
        let outcome = match self.expected {
            Some(expected) if self.delivered < expected => "aborted",
            _ => "complete",
        };
        pintail_log::emit(&format!(
            "{} sent={}ms {}B {outcome}",
            self.line,
            self.started.elapsed().as_millis(),
            self.delivered
        ));
    }
}

impl http_body::Body for TimedBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        let polled = std::pin::Pin::new(&mut this.inner).poll_frame(context);
        match &polled {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.delivered = this.delivered.saturating_add(data.len() as u64);
                }
            }
            std::task::Poll::Ready(None) => this.report(),
            _ => {}
        }
        polled
    }

    // Delegated so the response keeps its Content-Length: a body of unknown
    // length would switch to chunked encoding, which is a behaviour change
    // this measurement has no business making.
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TimedBody {
    fn drop(&mut self) {
        // Also the normal path for a body hyper consumed without polling
        // to its final frame; `report` decides which it was from the byte
        // count rather than assuming.
        self.report();
    }
}

/// The hostnames the wire certificate should cover.
///
/// Exported so the binary resolves them exactly as the settings API reports
/// them: an operator reading one number and the certificate carrying another
/// is the kind of disagreement nobody debugs quickly.
#[must_use]
pub fn wire_tls_hostnames(metadata: &pintail_meta::MetaStore) -> Vec<String> {
    crate::wire_certificate::configured_hostnames(metadata)
}

/// Every embedded dashboard asset path, for tests that need to name a real
/// one: the bundles are content-hashed, so their names change per build.
#[must_use]
pub fn dashboard_asset_paths() -> Vec<String> {
    Dashboard::iter().map(|path| path.to_string()).collect()
}

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
            axum::routing::delete(remove_workspace_member).patch(change_workspace_member_role),
        )
        .route("/workspaces/invites", get(list_invites).post(create_invite))
        .route(
            "/workspaces/invites/{id}",
            axum::routing::delete(revoke_invite),
        )
        .route("/workspaces/audit-log", get(workspace_audit_log))
        .route(
            "/settings/oauth/google",
            get(get_google_settings).put(put_google_settings),
        )
        .route("/settings/oauth/google/link", post(google_link_start))
        .route("/databases", get(list_databases).post(create_database))
        .route("/databases/local", post(create_local_database))
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
        .route("/vitals", get(crate::vitals::stream))
        .route("/wire/certificate", get(crate::wire_certificate::download))
        .route(
            "/settings/wire-tls",
            get(crate::wire_certificate::get_settings).put(crate::wire_certificate::put_settings),
        )
        .route("/ws", get(websocket))
        .route("/activity", get(activity))
        .route("/dlq", get(dead_letters))
        .route("/dlq/{id}", axum::routing::delete(discard_dead_letter))
        .route("/dlq/{id}/retry", post(retry_dead_letter))
        .route("/query", post(query))
        .route("/tables", get(list_tables))
        .route("/tables/columns", get(table_columns))
        .route("/tables/{name}/schema", get(table_schema))
        .route("/tables/{name}/data", get(table_data))
        .route("/tables/{name}/count", get(table_count))
        .route("/databases/{id}/snapshot", post(start_snapshot))
        .route("/databases/{id}/reset", post(snapshot::reset))
        .route("/databases/{id}/snapshot/status", get(snapshot_status))
        .route("/databases/{id}/tables/{name}/resync", post(resync))
        .route("/databases/{id}/tables/{name}/reconcile", post(reconcile))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    // Access logging is applied to /api only. The dashboard asset routes and
    // /health would otherwise bury every real request: a container health
    // check polls constantly and tells nobody anything.
    let api = Router::new()
        .route("/auth/setup/status", get(setup_status))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/google/start", get(google_start))
        .route("/auth/google/callback", get(google_callback))
        .route("/auth/google/exchange", post(google_exchange))
        .route("/auth/google/status", get(google_status))
        .route("/invites/status", get(invite_status))
        .merge(protected);

    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .nest("/api", api)
        .route("/", get(dashboard))
        .route("/{*path}", get(dashboard_asset))
        // Every request, not just the API's. The dashboard makes ~90 asset
        // requests per load and they were entirely invisible here, which is
        // precisely where an unexplained delay hides: the API log can say
        // 3ms while a client waits 40 seconds, and without the asset lines
        // there is no way to tell which side is lying.
        // The dashboard is JavaScript, CSS and JSON - all text, all several
        // times smaller compressed. Uncompressed they were shipped in full
        // to every visitor on every visit, which is minutes of transfer on
        // a high-latency link and the difference between a usable dashboard
        // and an unusable one. Applied last so it wraps every route,
        // including the embedded assets and the API's JSON.
        .layer(CompressionLayer::new())
        // OUTSIDE compression deliberately. Inside it, the body timer stops
        // when the compressor consumes the raw body, which is not when the
        // client has anything; outside, `sent=` covers the compressed bytes
        // actually handed to the socket.
        .layer(middleware::from_fn(access_log))
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

async fn dashboard(headers: header::HeaderMap) -> Response {
    embedded_asset_for("index.html", Some(&headers))
}

async fn dashboard_asset(Path(path): Path<String>, headers: header::HeaderMap) -> Response {
    // Real asset files (hashed JS/CSS, favicon, fonts) live at their exact
    // embedded path and 404 if truly missing. Route-like paths with no
    // extension are Nuxt pages: nuxt generate prerenders known routes as
    // `{path}/index.html`, but a dynamic route like `/databases/{id}` has
    // no prerendered file, so it falls through to the SPA shell (Nitro's
    // own `200.html`) and Vue Router resolves it client-side.
    if std::path::Path::new(&path).extension().is_some() {
        return embedded_asset_for(&path, Some(&headers));
    }
    let nested_index = format!("{path}/index.html");
    if Dashboard::get(&nested_index).is_some() {
        return embedded_asset_for(&nested_index, Some(&headers));
    }
    embedded_asset_for("200.html", Some(&headers))
}

/// Serves one embedded asset, answering `304` when the client already holds
/// it. Without this the `ETag` was decorative: a revalidating client - which
/// is every client for the non-immutable HTML shells - was sent the whole
/// body again regardless.
fn embedded_asset_for(path: &str, request_headers: Option<&header::HeaderMap>) -> Response {
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

    // Nuxt content-hashes every build artefact under `_nuxt/`, so a given
    // URL's bytes never change: it can be cached forever, and a new build
    // is a new URL. Everything else - the HTML shells above all - must be
    // revalidated, or a deploy would serve last release's page shell out of
    // a browser cache indefinitely.
    //
    // Without this the dashboard refetched all of its chunks on EVERY load:
    // a captured trace of one page view showed 72 `_nuxt/*` requests, none
    // cacheable, against an origin that was taking tens of seconds per
    // asset. The bytes were never the problem - the round trips were.
    // `_nuxt/builds/` is the exception inside an otherwise content-hashed
    // tree: `builds/latest.json` keeps a STABLE name and is how Nuxt notices
    // a new deployment. Marking it immutable pinned every browser to the
    // build it first saw, for a year - a far worse bug than the slow load
    // this caching was added to fix.
    let cache_control = if path.starts_with("_nuxt/") && !path.starts_with("_nuxt/builds/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    // A strong validator lets a cache revalidate the non-immutable shells
    // with a 304 instead of resending them, and lets any CDN in front hold
    // the immutable ones.
    let etag = format!("\"{}\"", hex_digest(&asset.metadata.sha256_hash()));

    let unchanged = request_headers
        .and_then(|headers| headers.get(header::IF_NONE_MATCH))
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag));
    if unchanged {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .expect("static response is valid");
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ETAG, etag)
        .body(Body::from(asset.data.into_owned()))
        .expect("static response is valid")
}

/// Renders an embedded asset's content hash as hex, for the `ETag`.
fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    digest.iter().fold(String::new(), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}
