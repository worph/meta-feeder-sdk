//! The one error type every transport HTTP surface answers with.
//!
//! Moved verbatim from meta-share `crates/meta-share/src/api/errors.rs`. It lives
//! here, not in the hull, because a plugin's error body is relayed to meta-share's
//! public API unchanged: the JSON shape `{ "error": "<message>" }` and the
//! status per variant are the contract every dashboard and peer depends on, and
//! the hull and its plugins must not be able to drift on it.

use axum::{http::StatusCode, response::IntoResponse, Json};

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound,
    SwarmGone,
    /// This peer is consumer-only (no `META_CORE_URL`) so it can't satisfy a
    /// raw-bytes request directly. The cross-peer proxy turns this into a
    /// clearer 502 with hint text; the direct route returns 503.
    ConsumerOnly,
    Upstream(String),
    /// Gateway-side compute exceeded its deadline. Distinct from `Upstream` so
    /// the dashboard can surface a clearer "the source fetcher is slow" message
    /// instead of a generic 502.
    GatewayTimeout(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            ApiError::SwarmGone => (
                StatusCode::SERVICE_UNAVAILABLE,
                "swarm task is gone".into(),
            ),
            ApiError::ConsumerOnly => (
                StatusCode::SERVICE_UNAVAILABLE,
                "this peer has no META_CORE_URL; raw bytes are only served by the source peer".into(),
            ),
            ApiError::Upstream(m) => (StatusCode::BAD_GATEWAY, m),
            ApiError::GatewayTimeout(m) => (StatusCode::GATEWAY_TIMEOUT, m),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::BadRequest(m) => write!(f, "bad request: {m}"),
            ApiError::NotFound => write!(f, "not found"),
            ApiError::SwarmGone => write!(f, "swarm task is gone"),
            ApiError::ConsumerOnly => write!(f, "consumer-only peer"),
            ApiError::Upstream(m) => write!(f, "upstream: {m}"),
            ApiError::GatewayTimeout(m) => write!(f, "timeout: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}
