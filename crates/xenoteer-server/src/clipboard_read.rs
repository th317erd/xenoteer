//! Authenticated, generation-fenced clipboard-read transport.

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
    ClipboardReadDelivery, ClipboardReadRequest, ClipboardReadResult, ClipboardTarget,
    DesktopGeneration, DesktopId, RequestId,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{ControlPlaneError, control_problem, validate_generation},
    problem::ApiProblem,
};

const CACHE_CONTROL_PRIVATE_NO_STORE: &str = "private, no-store";

/// Boxed future used by the object-safe clipboard-read boundary.
pub type ClipboardReadFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Authenticated and generation-fenced context supplied to a clipboard read.
#[derive(Debug, Clone)]
pub struct ClipboardReadRequestContext {
    principal: Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

impl ClipboardReadRequestContext {
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

    /// Returns the authenticated principal, including the checked clipboard grant.
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

/// Async-safe boundary between HTTP admission and the X11 clipboard actor.
///
/// Implementations must keep clipboard bytes out of diagnostics, revalidate the
/// supplied desktop lifetime before reading, and preserve request correlation
/// through any actor and artifact-store calls. The HTTP boundary independently
/// validates every result before publishing it.
pub trait ClipboardReadService: Send + Sync + 'static {
    /// Reads one validated selection representation.
    fn read<'a>(
        &'a self,
        context: ClipboardReadRequestContext,
        request: ClipboardReadRequest,
    ) -> ClipboardReadFuture<'a, Result<ClipboardReadResult, ControlPlaneError>>;
}

pub(crate) type SharedClipboardReadService = Arc<dyn ClipboardReadService>;

#[derive(Debug)]
pub(crate) struct UnavailableClipboardReadService;

impl ClipboardReadService for UnavailableClipboardReadService {
    fn read<'a>(
        &'a self,
        _: ClipboardReadRequestContext,
        _: ClipboardReadRequest,
    ) -> ClipboardReadFuture<'a, Result<ClipboardReadResult, ControlPlaneError>> {
        Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
    }
}

#[derive(Clone)]
struct ClipboardReadServiceState(SharedClipboardReadService);

pub(crate) fn routes(service: SharedClipboardReadService) -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/desktops/{desktop_id}/clipboard/read",
            post(read_clipboard),
        )
        .layer(Extension(ClipboardReadServiceState(service)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardReadQuery {
    desktop_generation: DesktopGeneration,
}

async fn read_clipboard(
    State(state): State<ApiState>,
    Extension(service): Extension<ClipboardReadServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ClipboardReadQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<ClipboardReadRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::ClipboardRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Query(query)), Ok(Json(request))) = (path, query, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if request.validate().is_err() {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) =
        validate_generation(&state, desktop_id, query.desktop_generation, request_id)
    {
        return problem.into_response();
    }

    let context = ClipboardReadRequestContext::new(
        principal,
        request_id,
        desktop_id,
        query.desktop_generation,
    );
    match service.0.read(context, request.clone()).await {
        Ok(result)
            if result_matches_request(&result, &request, desktop_id, query.desktop_generation) =>
        {
            tracing::info!(
                request_id = %request_id,
                desktop_id = %desktop_id,
                desktop_generation = %query.desktop_generation,
                selection = ?result.selection,
                target = result.evidence.target.as_str(),
                content_length = result.evidence.content_length,
                delivery = delivery_kind(&result.content),
                "clipboard read completed"
            );
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn result_matches_request(
    result: &ClipboardReadResult,
    request: &ClipboardReadRequest,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> bool {
    result
        .validate_for_desktop(desktop_id, desktop_generation)
        .is_ok()
        && result.selection == request.selection
        && target_matches_request(&result.evidence.target, request)
        && delivery_matches_target(result, request)
}

fn target_matches_request(target: &ClipboardTarget, request: &ClipboardReadRequest) -> bool {
    if request.preferred_targets.is_empty() {
        is_text_target(target)
    } else {
        request.preferred_targets.contains(target)
    }
}

fn delivery_matches_target(result: &ClipboardReadResult, request: &ClipboardReadRequest) -> bool {
    let target = &result.evidence.target;
    match &result.content {
        ClipboardReadDelivery::InlineText { .. } => is_text_target(target),
        ClipboardReadDelivery::InlineBinary { .. } => {
            is_binary_target(target) || request.allow_binary_fallback
        }
        ClipboardReadDelivery::Artifact { artifact } => {
            let media_type = artifact.content_type.as_str();
            match target.as_str() {
                "application/octet-stream" => media_type == "application/octet-stream",
                "image/png" => media_type == "image/png",
                _ if is_text_target(target) => {
                    is_plain_text_media_type(media_type)
                        || (request.allow_binary_fallback
                            && media_type == "application/octet-stream")
                }
                _ => false,
            }
        }
    }
}

fn is_plain_text_media_type(media_type: &str) -> bool {
    media_type == "text/plain" || media_type.starts_with("text/plain;")
}

fn is_text_target(target: &ClipboardTarget) -> bool {
    matches!(
        target.as_str(),
        "UTF8_STRING" | "text/plain;charset=utf-8" | "text/plain" | "STRING"
    )
}

fn is_binary_target(target: &ClipboardTarget) -> bool {
    matches!(target.as_str(), "application/octet-stream" | "image/png")
}

fn delivery_kind(delivery: &ClipboardReadDelivery) -> &'static str {
    match delivery {
        ClipboardReadDelivery::InlineText { .. } => "inline_text",
        ClipboardReadDelivery::InlineBinary { .. } => "inline_binary",
        ClipboardReadDelivery::Artifact { .. } => "artifact",
    }
}

fn json_no_store(body: ClipboardReadResult) -> Response {
    let mut response = (StatusCode::OK, Json(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE),
    );
    response
}
