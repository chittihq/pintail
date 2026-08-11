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
