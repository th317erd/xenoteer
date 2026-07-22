//! Authenticated, generation-fenced screenshot capture transport.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, RequestId, ScreenshotDelivery, ScreenshotRequest,
    ScreenshotResult, ScreenshotTarget,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{ControlPlaneError, control_problem, validate_generation},
    problem::ApiProblem,
};

const CACHE_CONTROL_PRIVATE_NO_STORE: &str = "private, no-store";

/// Boxed future used by the object-safe screenshot capture boundary.
pub type ScreenshotFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Authenticated and generation-fenced context supplied to screenshot capture.
#[derive(Debug, Clone)]
pub struct ScreenshotRequestContext {
    principal: Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

impl ScreenshotRequestContext {
    fn new(
        principal: Principal,
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Self {
        Self {
            principal,
            request_id,
            desktop_id,
            desktop_generation,
        }
    }

    /// Returns the authenticated principal, including the checked capture grant.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the server-assigned transport correlation identifier.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Returns the route-fenced desktop identifier.
    #[must_use]
    pub const fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    /// Returns the route-fenced desktop lifetime.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }
}

/// Async-safe boundary between HTTP admission and screenshot capture/storage.
///
/// Implementations must preserve the supplied request correlation internally,
/// revalidate generation-bound window identity immediately before capture, and
/// return only metadata for a private screenshot artifact. The HTTP boundary
/// independently validates every result before publishing it.
pub trait ScreenshotService: Send + Sync + 'static {
    /// Captures one validated request and stores its bounded output artifact.
    fn capture<'a>(
        &'a self,
        context: ScreenshotRequestContext,
        request: ScreenshotRequest,
    ) -> ScreenshotFuture<'a, Result<ScreenshotResult, ControlPlaneError>>;
}

pub(crate) type SharedScreenshotService = Arc<dyn ScreenshotService>;

#[derive(Debug)]
pub(crate) struct UnavailableScreenshotService;

impl ScreenshotService for UnavailableScreenshotService {
    fn capture<'a>(
        &'a self,
        _: ScreenshotRequestContext,
        _: ScreenshotRequest,
    ) -> ScreenshotFuture<'a, Result<ScreenshotResult, ControlPlaneError>> {
        Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
    }
}

#[derive(Clone)]
struct ScreenshotServiceState(SharedScreenshotService);

pub(crate) fn routes(service: SharedScreenshotService) -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/desktops/{desktop_id}/screenshots",
            post(capture_screenshot),
        )
        .layer(Extension(ScreenshotServiceState(service)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenshotQuery {
    desktop_generation: DesktopGeneration,
}

async fn capture_screenshot(
    State(state): State<ApiState>,
    Extension(service): Extension<ScreenshotServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ScreenshotQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<ScreenshotRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::CaptureRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Query(query)), Ok(Json(request))) = (path, query, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if request
        .validate_for_desktop(desktop_id, query.desktop_generation)
        .is_err()
    {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) =
        validate_generation(&state, desktop_id, query.desktop_generation, request_id)
    {
        return problem.into_response();
    }

    let context =
        ScreenshotRequestContext::new(principal, request_id, desktop_id, query.desktop_generation);
    match service.0.capture(context, request.clone()).await {
        Ok(result)
            if result_matches_request(&result, &request, desktop_id, query.desktop_generation) =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn result_matches_request(
    result: &ScreenshotResult,
    request: &ScreenshotRequest,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> bool {
    if result
        .validate_for_desktop(desktop_id, desktop_generation)
        .is_err()
        || result.target != request.target
        || result.format != request.format
        || result.cursor.requested != request.include_cursor
        || request.validate_for_source(result.source_size).ok() != Some(result.size)
    {
        return false;
    }
    match (&request.target, request.region) {
        (ScreenshotTarget::Root, Some(region)) if result.source_region.rect != region => {
            return false;
        }
        (ScreenshotTarget::Root, None)
            if result.source_region.rect.origin().x() != 0
                || result.source_region.rect.origin().y() != 0 =>
        {
            return false;
        }
        _ => {}
    }
    let ScreenshotDelivery::Artifact { artifact } = &result.delivery else {
        return false;
    };
    if request
        .max_bytes
        .is_some_and(|maximum| artifact.content_length > maximum)
    {
        return false;
    }
    if let Some(scale) = request.scale
        && (scale
            .width
            .is_some_and(|width| width != result.size.width())
            || scale
                .height
                .is_some_and(|height| height != result.size.height()))
    {
        return false;
    }
    true
}

fn json_no_store(body: ScreenshotResult) -> Response {
    let mut response = (StatusCode::OK, Json(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE),
    );
    response
}
