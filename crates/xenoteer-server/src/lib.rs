//! Xenoteer's HTTP composition boundary.

#![forbid(unsafe_code)]

mod health;
mod readiness;

use std::future::Future;

use axum::{Router, http::Request, routing::get};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

pub use readiness::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot};

/// Builds the Phase-0 HTTP router.
///
/// Only deliberately unauthenticated coarse health endpoints exist in Phase 0.
/// Later `/v1` routes will be composed behind authentication middleware.
pub fn router(readiness: ReadinessHandle) -> Router {
    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .with_state(readiness)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                tracing::info_span!(
                    "http.request",
                    request_id = %uuid::Uuid::new_v4(),
                    method = %request.method(),
                    path = request.uri().path(),
                )
            }),
        )
}

/// Serves a prepared router until graceful shutdown completes.
pub async fn serve(
    listener: TcpListener,
    application: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, application)
        .with_graceful_shutdown(shutdown)
        .await
}
