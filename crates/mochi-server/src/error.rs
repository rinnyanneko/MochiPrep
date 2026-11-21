//! The `{ "error": string }` body every failure carries, as Node's `setErrorHandler` sends it.
//! A failure the web UI must tell apart also carries a machine-readable `code`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    code: Option<&'static str>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
        }
    }

    /// An order that never reached OKX, so the client may correct and resubmit it
    /// under a new key instead of resolving the old one by lookup.
    pub const NOT_DISPATCHED: &'static str = "NOT_DISPATCHED";

    pub fn not_dispatched(message: impl Into<String>) -> Self {
        Self {
            code: Some(Self::NOT_DISPATCHED),
            ..Self::conflict(message)
        }
    }

    pub fn code(&self) -> Option<&'static str> {
        self.code
    }

    /// Node's default: any plain `throw new Error(..)` is a 400.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    /// A missing or wrong session token. The web client reads 401 as "fetch a new token", so
    /// nothing else (a wrong vault password, say) may use this status.
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized")
    }

    /// Fastify's own message for a body over `bodyLimit`.
    pub fn payload_too_large() -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large")
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Node logs server-side failures; messages here are already free of secrets.
        if self.status.is_server_error() {
            tracing::error!(status = self.status.as_u16(), message = %self.message);
        }
        let body = match self.code {
            Some(code) => serde_json::json!({ "error": self.message, "code": code }),
            None => serde_json::json!({ "error": self.message }),
        };
        (self.status, Json(body)).into_response()
    }
}
