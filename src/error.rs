//! The daemon's error type.
//!
//! Two rules hold here, and both exist because they were violated before:
//!
//! 1. **Internal detail never reaches the wire.** Every variant that isn't a
//!    deliberate client-facing message renders as a flat `internal server
//!    error` and logs the real cause. Previously the catch-all arm sent
//!    `self.to_string()`, which meant OAuth exchange failures and raw SQLite
//!    messages — SQL text, column names — were served to the browser.
//! 2. **Causes are preserved, not stringified.** `Other(String)` used to
//!    swallow everything including `anyhow` chains, so `source()` was always
//!    `None` and a log line showed only the outermost message.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("too many concurrent connections")]
    TooManyConnections,

    // --- Internal: message is logged, never returned to the client. ---
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Auth is not configured, but a route that requires it was reached.
    #[error("auth is not configured")]
    AuthNotConfigured,
    /// Malformed `~/.blueprint/env`, or an endpoint URL in it that won't parse.
    #[error("configuration: {0}")]
    Config(String),
    /// Session store / cookie layer failure.
    #[error("session: {0}")]
    Session(String),
    /// The OAuth token exchange or the GitHub profile fetch failed. Carries the
    /// cause for logging; the client is told nothing beyond a 500.
    #[error("identity provider: {0}")]
    Upstream(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Kept for the `anyhow` boundary at the CLI/daemon seam. `#[from]` +
    /// `transparent` preserves the whole chain, unlike the old `to_string()`.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AppError {
    /// Anything the client is allowed to read, paired with its status.
    /// `None` means "internal" — log it, return a generic 500.
    fn client_facing(&self) -> Option<(StatusCode, String)> {
        match self {
            AppError::NotFound => Some((StatusCode::NOT_FOUND, self.to_string())),
            AppError::BadRequest(_) => Some((StatusCode::BAD_REQUEST, self.to_string())),
            AppError::Unauthorized => Some((StatusCode::UNAUTHORIZED, self.to_string())),
            AppError::TooManyConnections => {
                Some((StatusCode::SERVICE_UNAVAILABLE, self.to_string()))
            }
            _ => None,
        }
    }

    pub fn upstream<E>(e: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        AppError::Upstream(Box::new(e))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self.client_facing() {
            Some((status, msg)) => (status, msg).into_response(),
            None => {
                // `?` on the Debug repr so `#[source]` chains show up in the log
                // — that context is the whole reason it's kept off the wire.
                tracing::error!(error = ?self, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
                    .into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(e: AppError) -> (StatusCode, String) {
        let r = e.into_response();
        let status = r.status();
        let bytes = to_bytes(r.into_body(), 64 * 1024).await.expect("read body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The whole point of `client_facing`: a database error must not describe the
    /// database to whoever provoked it.
    #[tokio::test]
    async fn internal_errors_do_not_leak_their_message() {
        let leaky = AppError::Sqlite(rusqlite::Error::InvalidQuery);
        let rendered = leaky.to_string();
        let (status, body) = body_of(AppError::Sqlite(rusqlite::Error::InvalidQuery)).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "internal server error");
        assert_ne!(body, rendered, "the internal message must not be the body");
    }

    #[tokio::test]
    async fn upstream_errors_do_not_leak_their_message() {
        let io = std::io::Error::other("token endpoint said no: client_secret=hunter2");
        let (status, body) = body_of(AppError::upstream(io)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !body.contains("hunter2"),
            "upstream detail leaked into the response: {body}"
        );
    }

    #[tokio::test]
    async fn client_facing_errors_keep_their_message() {
        let (status, body) = body_of(AppError::BadRequest("state mismatch".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "bad request: state mismatch");

        let (status, _) = body_of(AppError::NotFound).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = body_of(AppError::Unauthorized).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = body_of(AppError::TooManyConnections).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// `Other(String)` used to flatten `anyhow` chains; `#[from]` must keep them.
    #[test]
    fn anyhow_conversion_preserves_the_source_chain() {
        let root = std::io::Error::other("disk on fire");
        let chained = anyhow::Error::new(root).context("opening the store");
        let err = AppError::from(chained);

        let debug = format!("{err:?}");
        assert!(debug.contains("opening the store"), "lost context: {debug}");
        assert!(debug.contains("disk on fire"), "lost root cause: {debug}");
    }
}
