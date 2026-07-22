//! Adversarial HTTP tests for authenticated screenshot capture admission.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;
use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, ArtifactRef, CoordinateSpace,
    CursorCaptureEvidence, DesktopGeneration, DesktopId, Rect, RequestId, ScreenshotDelivery,
    ScreenshotFormat, ScreenshotRequest, ScreenshotResult, ScreenshotSourceLimitation,
    ScreenshotTarget, Sha256Digest, Size, Timestamp, WindowIdentityHash, WindowRect, WindowRef,
};

use crate::{
    AllowedOrigins, ApiServices, Authentication, ControlPlaneError, DesktopReadiness, Grant,
    Principal, ReadinessHandle, ReadinessSnapshot, ScreenshotFuture, ScreenshotRequestContext,
    ScreenshotService, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
    api_router_with_services, control::UnavailableControlPlane,
    observation::UnavailableObservationPlane,
};

const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone)]
struct CapturedCall {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    request: ScreenshotRequest,
}

struct FixtureScreenshotService {
    result: ScreenshotResult,
    error: Option<ControlPlaneError>,
    calls: AtomicUsize,
    captured: Mutex<Vec<CapturedCall>>,
}

impl FixtureScreenshotService {
    fn returning(result: ScreenshotResult) -> Self {
        Self {
            result,
            error: None,
            calls: AtomicUsize::new(0),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn failing(result: ScreenshotResult, error: ControlPlaneError) -> Self {
        Self {
            result,
            error: Some(error),
            calls: AtomicUsize::new(0),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn captured(&self) -> Vec<CapturedCall> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ScreenshotService for FixtureScreenshotService {
    fn capture<'a>(
        &'a self,
        context: ScreenshotRequestContext,
        request: ScreenshotRequest,
    ) -> ScreenshotFuture<'a, Result<ScreenshotResult, ControlPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedCall {
                principal_id: context.principal().id().to_owned(),
                request_id: context.request_id(),
                desktop_id: context.desktop_id(),
                desktop_generation: context.desktop_generation(),
                request,
            });
        Box::pin(async move {
            match self.error {
                Some(error) => Err(error),
                None => Ok(self.result.clone()),
            }
        })
    }
}

fn application(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    principal: Principal,
    screenshots: Arc<dyn ScreenshotService>,
) -> Result<Router, Box<dyn std::error::Error>> {
    let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
        DesktopReadiness::Ready,
        Some(generation),
        None::<String>,
    ));
    let provider = StaticTokenProvider::single(TOKEN, principal)?;
    let services = ApiServices::new(
        Arc::new(UnavailableControlPlane),
        Arc::new(UnavailableObservationPlane),
    )
    .with_screenshot_service(screenshots);
    Ok(api_router_with_services(
        readiness,
        desktop_id,
        Authentication::bearer(provider),
        StaticCapabilityProvider::empty()?,
        TransportLimits::default(),
        AllowedOrigins::default(),
        services,
    ))
}

fn capture_request(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    body: Vec<u8>,
) -> Result<Request<Body>, axum::http::Error> {
    Request::post(format!(
        "/v1/desktops/{desktop_id}/screenshots?desktop_generation={generation}"
    ))
    .header(
        header::AUTHORIZATION,
        "Bearer 0123456789abcdef0123456789abcdef",
    )
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body))
}

fn valid_request() -> ScreenshotRequest {
    ScreenshotRequest {
        target: ScreenshotTarget::Root,
        region: None,
        format: ScreenshotFormat::Png,
        include_cursor: false,
        scale: None,
        max_bytes: Some(1_024),
    }
}

fn artifact_result(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
) -> Result<ScreenshotResult, Box<dyn std::error::Error>> {
    Ok(ScreenshotResult {
        target: ScreenshotTarget::Root,
        source_region: WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 2, 2)?)?,
        source_size: Size::new(2, 2)?,
        limitation: ScreenshotSourceLimitation::RootVisibleFramebuffer,
        format: ScreenshotFormat::Png,
        size: Size::new(2, 2)?,
        raw: None,
        cursor: CursorCaptureEvidence {
            requested: false,
            composited: false,
            serial_before: None,
            serial_after: None,
            moved_during_capture: false,
        },
        sha256: Sha256Digest::new(DIGEST)?,
        delivery: ScreenshotDelivery::Artifact {
            artifact: ArtifactRef {
                artifact_id: ArtifactId::new(),
                purpose: ArtifactPurpose::Screenshot,
                desktop_id,
                desktop_generation: generation,
                content_type: ArtifactContentType::new("image/png")?,
                content_length: 4,
                sha256: Sha256Digest::new(DIGEST)?,
                created_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
                expires_at: Timestamp::parse("2026-07-21T01:00:00Z")?,
            },
        },
    })
}

#[tokio::test]
async fn valid_capture_dispatches_bound_context_and_returns_private_artifact_json()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let request = valid_request();
    let expected = artifact_result(desktop_id, generation)?;
    let service = Arc::new(FixtureScreenshotService::returning(expected.clone()));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let response = application(desktop_id, generation, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            generation,
            serde_json::to_vec(&request)?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL),
        Some(&"private, no-store".parse()?)
    );
    let body = to_bytes(response.into_body(), 64 * 1024).await?;
    assert_eq!(serde_json::from_slice::<ScreenshotResult>(&body)?, expected);
    assert_eq!(service.call_count(), 1);
    let captured = service.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].principal_id, "capture-reader");
    assert!(!captured[0].request_id.as_uuid().is_nil());
    assert_eq!(captured[0].desktop_id, desktop_id);
    assert_eq!(captured[0].desktop_generation, generation);
    assert_eq!(captured[0].request, request);
    Ok(())
}

#[tokio::test]
async fn missing_capture_grant_never_dispatches() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureScreenshotService::returning(artifact_result(
        desktop_id, generation,
    )?));
    let principal = Principal::new("observer", [Grant::DesktopObserve])?;
    let response = application(desktop_id, generation, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            generation,
            serde_json::to_vec(&valid_request())?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn malformed_body_and_query_never_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureScreenshotService::returning(artifact_result(
        desktop_id, generation,
    )?));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let application = application(desktop_id, generation, principal, service.clone())?;

    let malformed = application
        .clone()
        .oneshot(capture_request(
            desktop_id,
            generation,
            br#"{"target":{"kind":"root"},"format":"png","include_cursor":false,"region":null,"scale":null,"max_bytes":1024,"unknown":true}"#.to_vec(),
        )?)
        .await?;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

    let missing_query = application
        .clone()
        .oneshot(
            Request::post(format!("/v1/desktops/{desktop_id}/screenshots"))
                .header(
                    header::AUTHORIZATION,
                    "Bearer 0123456789abcdef0123456789abcdef",
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&valid_request())?))?,
        )
        .await?;
    assert_eq!(missing_query.status(), StatusCode::BAD_REQUEST);

    let unknown_query = application
        .oneshot(
            Request::post(format!(
                "/v1/desktops/{desktop_id}/screenshots?desktop_generation={generation}&extra=true"
            ))
            .header(
                header::AUTHORIZATION,
                "Bearer 0123456789abcdef0123456789abcdef",
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&valid_request())?))?,
        )
        .await?;
    assert_eq!(unknown_query.status(), StatusCode::BAD_REQUEST);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn stale_generation_never_dispatches() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let current = DesktopGeneration::new();
    let stale = DesktopGeneration::new();
    let service = Arc::new(FixtureScreenshotService::returning(artifact_result(
        desktop_id, current,
    )?));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let response = application(desktop_id, current, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            stale,
            serde_json::to_vec(&valid_request())?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn window_target_scope_mismatch_never_dispatches() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureScreenshotService::returning(artifact_result(
        desktop_id, generation,
    )?));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let request = ScreenshotRequest {
        target: ScreenshotTarget::WindowDrawable {
            window: WindowRef {
                desktop_id: DesktopId::new(),
                desktop_generation: generation,
                xid: 1,
                observed_generation: 1,
                identity_hash: WindowIdentityHash::new(DIGEST)?,
            },
        },
        ..valid_request()
    };
    let response = application(desktop_id, generation, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            generation,
            serde_json::to_vec(&request)?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn inline_or_malformed_service_evidence_is_never_published()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;

    let mut inline = artifact_result(desktop_id, generation)?;
    inline.delivery = ScreenshotDelivery::InlineBody { content_length: 4 };
    let inline_service = Arc::new(FixtureScreenshotService::returning(inline));
    let inline_response = application(
        desktop_id,
        generation,
        principal.clone(),
        inline_service.clone(),
    )?
    .oneshot(capture_request(
        desktop_id,
        generation,
        serde_json::to_vec(&valid_request())?,
    )?)
    .await?;
    assert_eq!(inline_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(inline_service.call_count(), 1);

    let mut malformed = artifact_result(desktop_id, generation)?;
    malformed.cursor.composited = true;
    let malformed_service = Arc::new(FixtureScreenshotService::returning(malformed));
    let malformed_response =
        application(desktop_id, generation, principal, malformed_service.clone())?
            .oneshot(capture_request(
                desktop_id,
                generation,
                serde_json::to_vec(&valid_request())?,
            )?)
            .await?;
    assert_eq!(
        malformed_response.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(malformed_service.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn valid_service_evidence_must_match_the_admitted_request()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let mut mismatched = artifact_result(desktop_id, generation)?;
    mismatched.target = ScreenshotTarget::WindowDrawable {
        window: WindowRef {
            desktop_id,
            desktop_generation: generation,
            xid: 1,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new(DIGEST)?,
        },
    };
    mismatched.limitation = ScreenshotSourceLimitation::WindowDrawableObscuredUndefined;
    assert!(
        mismatched
            .validate_for_desktop(desktop_id, generation)
            .is_ok()
    );
    let service = Arc::new(FixtureScreenshotService::returning(mismatched));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let response = application(desktop_id, generation, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            generation,
            serde_json::to_vec(&valid_request())?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(service.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn typed_service_errors_use_control_problem_mapping() -> Result<(), Box<dyn std::error::Error>>
{
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureScreenshotService::failing(
        artifact_result(desktop_id, generation)?,
        ControlPlaneError::ResourceExhausted,
    ));
    let principal = Principal::new("capture-reader", [Grant::CaptureRead])?;
    let response = application(desktop_id, generation, principal, service.clone())?
        .oneshot(capture_request(
            desktop_id,
            generation,
            serde_json::to_vec(&valid_request())?,
        )?)
        .await?;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.call_count(), 1);
    Ok(())
}
