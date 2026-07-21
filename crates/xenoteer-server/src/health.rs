//! Minimal unauthenticated liveness and readiness endpoints.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::ReadinessHandle;

#[derive(Serialize)]
struct LivenessResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
}

pub(crate) async fn livez() -> impl IntoResponse {
    (StatusCode::OK, Json(LivenessResponse { status: "alive" }))
}

pub(crate) async fn readyz(State(readiness): State<ReadinessHandle>) -> impl IntoResponse {
    let snapshot = readiness.snapshot();
    let (status, label) = if snapshot.is_ready() {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    (status, Json(ReadinessResponse { status: label }))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{DesktopReadiness, ReadinessSnapshot, router};

    #[tokio::test]
    async fn liveness_is_unconditionally_probe_friendly() -> Result<(), Box<dyn std::error::Error>>
    {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Probing,
            None,
            Some("phase0_backend_probes_not_wired"),
        ));
        let response = router(readiness)
            .oneshot(Request::get("/livez").body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let bytes = to_bytes(response.into_body(), 1024).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(body["status"], "alive");
        Ok(())
    }

    #[tokio::test]
    async fn readiness_is_truthful_and_transition_driven() -> Result<(), Box<dyn std::error::Error>>
    {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::phase0_backend_probes_not_wired());
        let phase0_snapshot = readiness.snapshot();
        assert_eq!(phase0_snapshot.state, DesktopReadiness::Probing);
        assert_eq!(
            phase0_snapshot.reason_code.as_deref(),
            Some("phase0_backend_probes_not_wired")
        );
        assert!(!phase0_snapshot.is_ready());
        let unavailable = router(readiness.clone())
            .oneshot(Request::get("/readyz").body(Body::empty())?)
            .await?;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        let unavailable_body = to_bytes(unavailable.into_body(), 1024).await?;
        let unavailable_json: serde_json::Value = serde_json::from_slice(&unavailable_body)?;
        assert_eq!(unavailable_json, serde_json::json!({"status": "not_ready"}));

        readiness.transition(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(xenoteer_protocol::DesktopGeneration::new()),
            None::<String>,
        ));
        let available = router(readiness)
            .oneshot(Request::get("/readyz").body(Body::empty())?)
            .await?;
        assert_eq!(available.status(), StatusCode::OK);
        let available_body = to_bytes(available.into_body(), 1024).await?;
        let available_json: serde_json::Value = serde_json::from_slice(&available_body)?;
        assert_eq!(available_json, serde_json::json!({"status": "ready"}));
        Ok(())
    }
}
