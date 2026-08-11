use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// One client-visible API failure.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
    /// A short, stable name for *which* refusal this is.
    ///
    /// The Google callback answers every outcome with the same 303, carrying
    /// only a code in the URL, so without this the browser had to infer the
    /// reason from the HTTP status. Several very different refusals share a
    /// status - a spent invite, a revoked one, an account belonging to no
    /// workspace - and all rendered as "you were not invited", which is
    /// actively misleading when the invite exists and the user is looking
    /// right at it.
    auth_code: Option<&'static str>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    pub(crate) fn request_timeout(message: impl Into<String>) -> Self {
        Self::new(StatusCode::REQUEST_TIMEOUT, message)
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }

    /// Names this refusal for the sign-in redirect.
    pub(crate) const fn with_auth_code(mut self, code: &'static str) -> Self {
        self.auth_code = Some(code);
        self
    }

    pub(crate) const fn auth_code(&self) -> Option<&'static str> {
        self.auth_code
    }

    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            auth_code: None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}
