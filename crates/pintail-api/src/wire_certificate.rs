//! Serves the node's wire-protocol certificate for download.
//!
//! The public half only. The private key sits beside it in the data directory
//! and never leaves the process - which is exactly why the certificate *can*
//! be handed to anyone: it is the part clients are meant to have.
//!
//! Downloading it upgrades a client from "encrypted" to "encrypted and
//! verified". Without it a connection can still use TLS, but nothing proves
//! the server on the other end is this node rather than someone between.

use axum::{
    Extension,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::{ApiState, auth::AuthPrincipal, error::ApiError};

/// Returns the certificate as a PEM download.
pub(crate) async fn download(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Response, ApiError> {
    principal.require_scope("read")?;
    let path = state.data_dir()?.join("wire-cert.pem");
    let pem = std::fs::read_to_string(&path).map_err(|error| {
        ApiError::not_found(format!(
            "this node has no managed wire certificate: {error}. A certificate \
             configured through PINTAIL_WIRE_TLS_CERT is not served here, since \
             the operator who set it already has it."
        ))
    })?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-pem-file"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"pintail-ca.pem\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        pem,
    )
        .into_response())
}

/// The hostnames the certificate should answer to, and the ones it currently
/// does.
///
/// Those differ whenever the setting has been changed since the last restart,
/// which is exactly the state an operator needs to see: the certificate is
/// read once at boot, so saving a hostname does not reissue it.
#[derive(serde::Serialize)]
pub(crate) struct WireTlsSettings {
    /// What the operator asked for, comma-separated.
    hostnames: String,
    /// What the certificate on disk actually covers.
    active_names: Vec<String>,
    /// True when the two disagree and a restart would reconcile them.
    restart_required: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct PutWireTlsRequest {
    hostnames: String,
}

/// The hostname the dashboard is served on, used when nothing is configured.
///
/// An operator who has already told Pintail its public URL for Google sign-in
/// should not have to say it a second time to get a usable certificate.
fn public_hostname(metadata: &pintail_meta::MetaStore) -> Option<String> {
    let origin = metadata.setting("oauth_google_public_origin").ok()??;
    reqwest::Url::parse(&origin)
        .ok()?
        .host_str()
        .map(str::to_owned)
}

/// Resolves the configured hostnames, falling back to the public URL's host.
pub(crate) fn configured_hostnames(metadata: &pintail_meta::MetaStore) -> Vec<String> {
    let explicit = metadata
        .setting("wire_tls_hostnames")
        .ok()
        .flatten()
        .unwrap_or_default();
    let names: Vec<String> = explicit
        .split(',')
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        public_hostname(metadata).into_iter().collect()
    } else {
        names
    }
}

pub(crate) async fn get_settings(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<axum::Json<WireTlsSettings>, ApiError> {
    principal.require_admin()?;
    let metadata = state.metadata()?;
    let wanted = configured_hostnames(&metadata);
    // What the live certificate covers is recorded beside it at generation.
    let active_names: Vec<String> =
        std::fs::read_to_string(state.data_dir()?.join("wire-cert.names"))
            .map(|recorded| {
                recorded
                    .lines()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    let restart_required = !wanted
        .iter()
        .all(|name| active_names.iter().any(|active| active == name));
    Ok(axum::Json(WireTlsSettings {
        hostnames: wanted.join(", "),
        active_names,
        restart_required,
    }))
}

pub(crate) async fn put_settings(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    axum::Json(request): axum::Json<PutWireTlsRequest>,
) -> Result<axum::Json<WireTlsSettings>, ApiError> {
    principal.require_admin()?;
    // Only names a certificate can carry. A value with a scheme, a port or a
    // path produces a certificate that silently fails to verify, which is
    // worse than refusing it here.
    let cleaned: Vec<String> = request
        .hostnames
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    for name in &cleaned {
        if name.contains("://") || name.contains('/') || name.contains(':') {
            return Err(ApiError::bad_request(format!(
                "\"{name}\" is not a hostname: give the host only, without scheme, port or path"
            )));
        }
    }
    state
        .metadata()?
        .set_setting("wire_tls_hostnames", &cleaned.join(","))
        .map_err(ApiError::internal)?;
    crate::audit::record(
        &state,
        &principal,
        "wire_tls.update",
        None,
        Some(serde_json::json!({ "hostnames": cleaned })),
    );
    get_settings(Extension(principal), State(state)).await
}
