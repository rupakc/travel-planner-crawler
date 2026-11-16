//! API errors: every error is JSON `{"error": <code>, "message": <text>}`.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code: `unauthorized`, `bad_request`, `not_found`,
    /// `rate_limited`, `unavailable`, `internal`.
    pub error: String,
    /// Human-readable detail (never contains page content or tokens).
    pub message: String,
}

/// An error answered by a handler or middleware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Missing, malformed, unknown, disabled or wrong bearer token (401).
    Unauthorized,
    /// Invalid input (400).
    BadRequest(String),
    /// Unknown, tombstoned or not visible to this client (404).
    NotFound,
    /// The client's rate limit is exhausted (429 + `Retry-After` seconds).
    RateLimited {
        /// Seconds until a request will be accepted again.
        retry_after: u64,
    },
    /// A dependency is not ready (503).
    Unavailable,
    /// Anything else (500).
    Internal,
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a valid bearer token is required".into(),
            ),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".into()),
            ApiError::RateLimited { retry_after } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                format!("rate limit exceeded; retry in {retry_after} s"),
            ),
            ApiError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "not ready".into(),
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error".into(),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        let mut resp = (
            status,
            Json(ErrorBody {
                error: code.into(),
                message,
            }),
        )
            .into_response();
        match self {
            ApiError::Unauthorized => {
                resp.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            ApiError::RateLimited { retry_after } => {
                resp.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
            }
            _ => {}
        }
        resp
    }
}

impl From<vc_core::Error> for ApiError {
    fn from(e: vc_core::Error) -> Self {
        match e {
            vc_core::Error::Invalid(m) => ApiError::BadRequest(m),
            vc_core::Error::NotFound(_) => ApiError::NotFound,
            other => {
                // vc-core errors never carry row content or tokens; log the class and message.
                tracing::error!(class = ?other.class(), error = %other, "read model error");
                ApiError::Internal
            }
        }
    }
}
