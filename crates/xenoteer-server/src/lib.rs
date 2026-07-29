//! Xenoteer's HTTP composition boundary.

#![forbid(unsafe_code)]

mod abuse;
mod accessibility;
#[cfg(test)]
mod accessibility_tests;
mod artifacts;
mod auth;
#[cfg(test)]
mod auth_input_tests;
mod clipboard_read;
mod control;
mod health;
mod limits;
mod observation;
mod problem;
mod protocol_version;
mod readiness;
mod screenshot;
mod status;
mod viewer;
mod viewer_gateway;
mod websocket;

#[cfg(test)]
mod clipboard_read_tests;

#[cfg(test)]
mod screenshot_tests;

#[cfg(test)]
mod viewer_gateway_tests;

use std::{future::Future, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Request as AxumRequest},
    http::Request,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use xenoteer_protocol::DesktopId;

pub use accessibility::{AccessibilityFuture, AccessibilityPlane, AccessibilityPlaneError};
pub use artifacts::{
    ARTIFACT_SHA256_HEADER, ArtifactAccessRequest, ArtifactDownload, ArtifactFuture,
    ArtifactPurposeSet, ArtifactRequestContext, ArtifactService, ArtifactUploadRequest,
};
pub use auth::{
    Authentication, Grant, GrantRequirement, Principal, PrincipalError, StaticTokenProvider,
    TokenLoadError, TokenMaterialError, TokenProvider, TokenProviderError,
    command_grant_requirement,
};
pub use clipboard_read::{ClipboardReadFuture, ClipboardReadRequestContext, ClipboardReadService};
pub use control::{
    CommandCancellation, CommandSubmission, CommandWait, ControlFuture, ControlPlane,
    ControlPlaneError, ControlRequestContext, EventReplay, EventSubscription, LiveEvent,
    LiveEventReceiver, MAX_COMMAND_WAIT_MS, SubmissionDisposition,
};
pub use limits::{
    DEFAULT_MAX_CONCURRENT_LONG_POLLS, DEFAULT_MAX_CONCURRENT_LONG_POLLS_PER_PRINCIPAL,
    TransportLimitError, TransportLimits,
};
pub use observation::{ObservationFuture, ObservationPlane};
pub use readiness::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot};
pub use screenshot::{ScreenshotFuture, ScreenshotRequestContext, ScreenshotService};
pub use status::{CapabilityProvider, StaticCapabilityProvider};
pub use viewer::{
    DEFAULT_VIEWER_TICKET_CAPACITY, InMemoryViewerTicketRegistry, MAX_VIEWER_TICKET_CAPACITY,
    VIEWER_WEBSOCKET_AUDIENCE, ViewerTicketClaims, ViewerTicketClock, ViewerTicketClockReading,
    ViewerTicketConsumeAudience, ViewerTicketConsumeRequest, ViewerTicketEntropy,
    ViewerTicketFuture, ViewerTicketIssueContext, ViewerTicketRegistryConfig,
    ViewerTicketRegistryError, ViewerTicketService,
};
pub use viewer_gateway::{
    DEFAULT_MAX_VIEWER_SESSIONS, LoopbackWebsockifyConnector, MAX_VIEWER_FRAME_BYTES,
    VIEWER_BINARY_PROTOCOL, VIEWER_TICKET_PROTOCOL_PREFIX, ViewerBackendConnection,
    ViewerBackendConnector, ViewerBackendError, ViewerBackendFuture, ViewerBackendMessage,
    ViewerGateway, ViewerGatewayConfigurationError, ViewerGatewayLimits,
};
pub use websocket::{AllowedOrigins, OriginPolicyError};

/// Replaceable service seams composed into the authenticated API router.
#[derive(Clone)]
pub struct ApiServices {
    control: Arc<dyn ControlPlane>,
    observation: Arc<dyn ObservationPlane>,
    accessibility: Arc<dyn AccessibilityPlane>,
    artifacts: Arc<dyn ArtifactService>,
    clipboard_reads: Arc<dyn ClipboardReadService>,
    screenshots: Arc<dyn ScreenshotService>,
    viewer_tickets: Arc<dyn ViewerTicketService>,
    viewer_gateway: Option<Arc<ViewerGateway>>,
}

impl ApiServices {
    /// Creates a service bundle with unavailable defaults for optional planes.
    #[must_use]
    pub fn new(control: Arc<dyn ControlPlane>, observation: Arc<dyn ObservationPlane>) -> Self {
        Self {
            control,
            observation,
            accessibility: Arc::new(accessibility::UnavailableAccessibilityPlane),
            artifacts: Arc::new(artifacts::UnavailableArtifactService),
            clipboard_reads: Arc::new(clipboard_read::UnavailableClipboardReadService),
            screenshots: Arc::new(screenshot::UnavailableScreenshotService),
            viewer_tickets: Arc::new(viewer::UnavailableViewerTicketService),
            viewer_gateway: None,
        }
    }

    /// Replaces semantic accessibility reading without changing other service seams.
    #[must_use]
    pub fn with_accessibility_plane(mut self, accessibility: Arc<dyn AccessibilityPlane>) -> Self {
        self.accessibility = accessibility;
        self
    }

    /// Replaces the artifact service without changing control or observation.
    #[must_use]
    pub fn with_artifact_service(mut self, artifacts: Arc<dyn ArtifactService>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Replaces clipboard reading without changing other service seams.
    #[must_use]
    pub fn with_clipboard_read_service(
        mut self,
        clipboard_reads: Arc<dyn ClipboardReadService>,
    ) -> Self {
        self.clipboard_reads = clipboard_reads;
        self
    }

    /// Replaces screenshot capture without changing other service seams.
    #[must_use]
    pub fn with_screenshot_service(mut self, screenshots: Arc<dyn ScreenshotService>) -> Self {
        self.screenshots = screenshots;
        self
    }

    /// Replaces ticket issuance without enabling or exposing a viewer gateway.
    #[must_use]
    pub fn with_viewer_ticket_service(
        mut self,
        viewer_tickets: Arc<dyn ViewerTicketService>,
    ) -> Self {
        self.viewer_tickets = viewer_tickets;
        self
    }

    /// Enables the public query-free viewer page and WebSocket gateway.
    #[must_use]
    pub fn with_viewer_gateway(mut self, viewer_gateway: Arc<ViewerGateway>) -> Self {
        self.viewer_gateway = Some(viewer_gateway);
        self
    }
}

#[derive(Clone)]
struct ApiState {
    readiness: ReadinessHandle,
    desktop_id: DesktopId,
    capabilities: status::SharedCapabilityProvider,
    limits: TransportLimits,
    origins: AllowedOrigins,
    control: control::SharedControlPlane,
    observation: observation::SharedObservationPlane,
    abuse: abuse::AbuseControls,
    long_polls: limits::LongPollAdmission,
}

/// Builds the Phase-0 HTTP router.
///
/// Only deliberately unauthenticated coarse health endpoints exist in Phase 0.
/// Later `/v1` routes will be composed behind authentication middleware.
pub fn router(readiness: ReadinessHandle) -> Router {
    with_trace(health_routes(readiness))
}

/// Builds health routes plus the authenticated, bounded version-one shell.
pub fn api_router(
    readiness: ReadinessHandle,
    desktop_id: DesktopId,
    authentication: Authentication,
    capabilities: impl CapabilityProvider,
    limits: TransportLimits,
    origins: AllowedOrigins,
) -> Router {
    api_router_with_control(
        readiness,
        desktop_id,
        authentication,
        capabilities,
        limits,
        origins,
        Arc::new(control::UnavailableControlPlane),
    )
}

/// Builds the authenticated API with a replaceable lease/command coordinator.
pub fn api_router_with_control(
    readiness: ReadinessHandle,
    desktop_id: DesktopId,
    authentication: Authentication,
    capabilities: impl CapabilityProvider,
    limits: TransportLimits,
    origins: AllowedOrigins,
    control: Arc<dyn ControlPlane>,
) -> Router {
    api_router_with_planes(
        readiness,
        desktop_id,
        authentication,
        capabilities,
        limits,
        origins,
        control,
        Arc::new(observation::UnavailableObservationPlane),
    )
}

/// Builds the authenticated API with replaceable control and observation actors.
#[allow(clippy::too_many_arguments)]
pub fn api_router_with_planes(
    readiness: ReadinessHandle,
    desktop_id: DesktopId,
    authentication: Authentication,
    capabilities: impl CapabilityProvider,
    limits: TransportLimits,
    origins: AllowedOrigins,
    control: Arc<dyn ControlPlane>,
    observation: Arc<dyn ObservationPlane>,
) -> Router {
    api_router_with_services(
        readiness,
        desktop_id,
        authentication,
        capabilities,
        limits,
        origins,
        ApiServices::new(control, observation),
    )
}

/// Builds the authenticated API with every service seam replaceable.
pub fn api_router_with_services(
    readiness: ReadinessHandle,
    desktop_id: DesktopId,
    authentication: Authentication,
    capabilities: impl CapabilityProvider,
    limits: TransportLimits,
    origins: AllowedOrigins,
    services: ApiServices,
) -> Router {
    let ApiServices {
        control,
        observation,
        accessibility,
        artifacts,
        clipboard_reads,
        screenshots,
        viewer_tickets,
        viewer_gateway,
    } = services;
    let abuse = abuse::AbuseControls::new();
    let state = ApiState {
        readiness: readiness.clone(),
        desktop_id,
        capabilities: Arc::new(capabilities),
        limits,
        origins,
        control,
        observation,
        abuse: abuse.clone(),
        long_polls: limits::LongPollAdmission::new(limits),
    };
    let public_viewer = viewer_gateway::routes(viewer_tickets.clone(), viewer_gateway)
        .layer(middleware::from_fn(assign_request_id))
        .with_state(state.clone());
    let versioned = Router::new()
        .route("/v1/status", get(status::status))
        .route("/v1/capabilities", get(status::capabilities))
        .route("/v1/ws", get(websocket::upgrade))
        .merge(control::routes())
        .merge(observation::routes())
        .merge(accessibility::routes(accessibility))
        .merge(artifacts::routes(artifacts))
        .merge(clipboard_read::routes(clipboard_reads))
        .merge(screenshot::routes(screenshots))
        .merge(viewer::routes(viewer_tickets))
        .fallback(status::not_found)
        .layer(DefaultBodyLimit::max(limits.max_body_bytes()))
        .layer(middleware::from_fn_with_state(
            auth::AuthenticationState::new(authentication, abuse),
            auth::require_authentication,
        ))
        .layer(middleware::from_fn_with_state(
            limits::AdmissionControl::new(limits),
            limits::enforce,
        ))
        .layer(middleware::from_fn(
            artifacts::preserve_upload_content_length,
        ))
        .layer(middleware::from_fn(assign_request_id))
        .with_state(state);

    with_trace(
        Router::new()
            .merge(health_routes(readiness))
            .merge(public_viewer)
            .merge(versioned),
    )
}

fn health_routes(readiness: ReadinessHandle) -> Router {
    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .with_state(readiness)
}

async fn assign_request_id(mut request: AxumRequest<Body>, next: Next) -> Response {
    if request
        .extensions()
        .get::<xenoteer_protocol::RequestId>()
        .is_none()
    {
        request
            .extensions_mut()
            .insert(xenoteer_protocol::RequestId::new());
    }
    next.run(request).await
}

fn with_trace(router: Router) -> Router {
    router.layer(
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
    axum::serve(
        listener,
        application.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

#[cfg(test)]
mod api_tests {
    use axum::{body::to_bytes, http::StatusCode};
    use tower::ServiceExt;
    use xenoteer_protocol::{DesktopGeneration, Problem};

    use super::*;

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    fn application() -> Result<Router, Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(DesktopGeneration::new()),
            None::<String>,
        ));
        let principal = Principal::local_operator()?;
        let provider = StaticTokenProvider::single(TOKEN, principal)?;
        Ok(api_router(
            readiness,
            DesktopId::new(),
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
        ))
    }

    #[tokio::test]
    async fn health_is_public_but_every_v1_route_requires_one_bearer()
    -> Result<(), Box<dyn std::error::Error>> {
        let live = application()?
            .oneshot(Request::get("/livez").body(Body::empty())?)
            .await?;
        assert_eq!(live.status(), StatusCode::OK);

        let missing = application()?
            .oneshot(Request::get("/v1/status").body(Body::empty())?)
            .await?;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            missing.headers().get(http::header::WWW_AUTHENTICATE),
            Some(&http::HeaderValue::from_static("Bearer realm=\"xenoteer\""))
        );

        let duplicate = application()?
            .oneshot(
                Request::get("/v1/status")
                    .header(http::header::AUTHORIZATION, "Bearer invalid")
                    .header(http::header::AUTHORIZATION, "Bearer invalid")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(duplicate.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn valid_bearer_reaches_bounded_status_shell() -> Result<(), Box<dyn std::error::Error>> {
        let authorization = format!("Bearer {}", std::str::from_utf8(TOKEN)?);
        let response = application()?
            .oneshot(
                Request::get("/v1/status")
                    .header(http::header::AUTHORIZATION, authorization)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(http::header::CACHE_CONTROL),
            Some(&http::HeaderValue::from_static("private, no-store"))
        );
        let body = to_bytes(response.into_body(), 16 * 1_024).await?;
        let mut status: xenoteer_protocol::StatusResponse = serde_json::from_slice(&body)?;
        status.validate()?;
        let body = serde_json::to_value(status)?;
        assert_eq!(
            body["protocol_min"],
            serde_json::json!({"major": 1, "minor": 0})
        );
        assert_eq!(body["desktop"]["state"], "ready");
        assert!(body["desktop"]["id"].as_str().is_some());
        assert!(body["server_time"].as_str().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn streamed_oversize_json_gets_structured_413() -> Result<(), Box<dyn std::error::Error>>
    {
        let authorization = format!("Bearer {}", std::str::from_utf8(TOKEN)?);
        let body = vec![b' '; TransportLimits::default().max_body_bytes() + 1];
        let response = application()?
            .oneshot(
                Request::post(format!("/v1/desktops/{}/commands", DesktopId::new()))
                    .header(http::header::AUTHORIZATION, authorization)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(http::header::TRANSFER_ENCODING, "chunked")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE),
            Some(&http::HeaderValue::from_static("application/problem+json"))
        );
        let body = to_bytes(response.into_body(), 16 * 1_024).await?;
        let problem: Problem = serde_json::from_slice(&body)?;
        problem.validate()?;
        assert_eq!(problem.status(), StatusCode::PAYLOAD_TOO_LARGE.as_u16());
        Ok(())
    }

    #[tokio::test]
    async fn router_fallback_auth_bucket_is_bounded_and_does_not_trust_proxy_headers()
    -> Result<(), Box<dyn std::error::Error>> {
        let application = application()?;
        for attempt in 0..10 {
            let response = application
                .clone()
                .oneshot(
                    Request::get("/v1/status")
                        .header(http::header::AUTHORIZATION, "Bearer invalid")
                        .header("x-forwarded-for", format!("192.0.2.{attempt}"))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let exhausted = application
            .clone()
            .oneshot(
                Request::get("/v1/status")
                    .header(http::header::AUTHORIZATION, "Bearer invalid")
                    .header("x-forwarded-for", "198.51.100.200")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(exhausted.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "resource_exhausted");

        let health = application
            .oneshot(Request::get("/livez").body(Body::empty())?)
            .await?;
        assert_eq!(health.status(), StatusCode::OK);
        Ok(())
    }
}
