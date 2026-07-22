//! Authenticated, generation-fenced window observation transport.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, RequestId, WindowListPage, WindowListRequest, WindowOrder,
    WindowPageCursor, WindowQueryPage, WindowQueryRequest, WindowReferenceToken,
    WindowResolveRequest, WindowResolveResult, WindowSnapshotRequest, WindowSnapshotResult,
    WindowSnapshotTarget, WindowWaitRequest, WindowWaitResult,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{ControlPlaneError, ControlRequestContext},
    problem::ApiProblem,
};

/// Boxed future used by the object-safe observation boundary.
pub type ObservationFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Read-only seam between HTTP handlers and the desktop observation actor.
pub trait ObservationPlane: Send + Sync + 'static {
    /// Returns one atomic, bounded page in caller-selected order.
    fn list_windows<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowListRequest,
    ) -> ObservationFuture<'a, Result<WindowListPage, ControlPlaneError>>;

    /// Resolves an opaque transport token to one exact live birth.
    fn window_snapshot<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowSnapshotRequest,
    ) -> ObservationFuture<'a, Result<WindowSnapshotResult, ControlPlaneError>>;

    /// Evaluates a bounded selector against one atomic model revision.
    fn query_windows<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowQueryRequest,
    ) -> ObservationFuture<'a, Result<WindowQueryPage, ControlPlaneError>>;

    /// Resolves a bounded selector without silently choosing an ambiguous target.
    fn resolve_window<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowResolveRequest,
    ) -> ObservationFuture<'a, Result<WindowResolveResult, ControlPlaneError>>;

    /// Atomically checks, registers, and rechecks one bounded observation wait.
    fn wait_window<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowWaitRequest,
    ) -> ObservationFuture<'a, Result<WindowWaitResult, ControlPlaneError>>;
}

pub(crate) type SharedObservationPlane = Arc<dyn ObservationPlane>;

#[derive(Debug)]
pub(crate) struct UnavailableObservationPlane;

impl ObservationPlane for UnavailableObservationPlane {
    fn list_windows<'a>(
        &'a self,
        _: ControlRequestContext,
        _: WindowListRequest,
    ) -> ObservationFuture<'a, Result<WindowListPage, ControlPlaneError>> {
        unavailable()
    }

    fn window_snapshot<'a>(
        &'a self,
        _: ControlRequestContext,
        _: WindowSnapshotRequest,
    ) -> ObservationFuture<'a, Result<WindowSnapshotResult, ControlPlaneError>> {
        unavailable()
    }

    fn query_windows<'a>(
        &'a self,
        _: ControlRequestContext,
        _: WindowQueryRequest,
    ) -> ObservationFuture<'a, Result<WindowQueryPage, ControlPlaneError>> {
        unavailable()
    }

    fn resolve_window<'a>(
        &'a self,
        _: ControlRequestContext,
        _: WindowResolveRequest,
    ) -> ObservationFuture<'a, Result<WindowResolveResult, ControlPlaneError>> {
        unavailable()
    }

    fn wait_window<'a>(
        &'a self,
        _: ControlRequestContext,
        _: WindowWaitRequest,
    ) -> ObservationFuture<'a, Result<WindowWaitResult, ControlPlaneError>> {
        unavailable()
    }
}

fn unavailable<'a, T>() -> ObservationFuture<'a, Result<T, ControlPlaneError>> {
    Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/v1/desktops/{desktop_id}/windows", get(list_windows))
        .route(
            "/v1/desktops/{desktop_id}/windows/query",
            post(query_windows),
        )
        .route(
            "/v1/desktops/{desktop_id}/windows/resolve",
            post(resolve_window),
        )
        .route("/v1/desktops/{desktop_id}/windows/wait", post(wait_window))
        .route(
            "/v1/desktops/{desktop_id}/windows/{reference_token}",
            get(window_snapshot),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowListQuery {
    desktop_generation: DesktopGeneration,
    limit: Option<u16>,
    order: Option<WindowOrder>,
    cursor: Option<WindowPageCursor>,
}

async fn list_windows(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<WindowListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Query(query))) = (path, query) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let request = WindowListRequest {
        desktop_id,
        desktop_generation: query.desktop_generation,
        limit: query
            .limit
            .unwrap_or(xenoteer_protocol::DEFAULT_WINDOW_PAGE_LIMIT),
        order: query.order.unwrap_or(WindowOrder::CreationAscending),
        cursor: query.cursor,
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    match state
        .observation
        .list_windows(context(principal, request_id), request)
        .await
    {
        Ok(page)
            if page.validate().is_ok()
                && page.desktop_id == desktop_id
                && page.desktop_generation == expected_generation =>
        {
            json_no_store(page)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => observation_problem(error, request_id).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowSnapshotQuery {
    desktop_generation: DesktopGeneration,
}

async fn window_snapshot(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, WindowReferenceToken)>, axum::extract::rejection::PathRejection>,
    query: Result<Query<WindowSnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path((desktop_id, token))), Ok(Query(query))) = (path, query) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let request = WindowSnapshotRequest {
        desktop_id,
        desktop_generation: query.desktop_generation,
        target: WindowSnapshotTarget::Token { token },
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    match state
        .observation
        .window_snapshot(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.window.snapshot.window.desktop_id == desktop_id
                && result.window.snapshot.window.desktop_generation == expected_generation =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => observation_problem(error, request_id).into_response(),
    }
}

async fn query_windows(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<WindowQueryRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    match state
        .observation
        .query_windows(context(principal, request_id), request)
        .await
    {
        Ok(page)
            if page.validate().is_ok()
                && page.desktop_id == desktop_id
                && page.desktop_generation == expected_generation =>
        {
            json_no_store(page)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => observation_problem(error, request_id).into_response(),
    }
}

async fn resolve_window(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<WindowResolveRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    match state
        .observation
        .resolve_window(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.desktop_id == desktop_id
                && result.desktop_generation == expected_generation =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => observation_problem(error, request_id).into_response(),
    }
}

async fn wait_window(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<WindowWaitRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let Some(_permit) = state.long_polls.try_acquire(principal.id()) else {
        return ApiProblem::resource_exhausted(request_id).into_response();
    };
    let expected_generation = request.desktop_generation;
    match state
        .observation
        .wait_window(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.desktop_id == desktop_id
                && result.desktop_generation == expected_generation =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => observation_problem(error, request_id).into_response(),
    }
}

fn validate_request(
    state: &ApiState,
    path_desktop: DesktopId,
    body_desktop: DesktopId,
    generation: DesktopGeneration,
    shape_valid: bool,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    if !shape_valid || body_desktop != path_desktop {
        return Err(ApiProblem::invalid_request(request_id));
    }
    super::control::validate_generation(state, path_desktop, generation, request_id)
}

fn context(principal: Principal, request_id: RequestId) -> ControlRequestContext {
    ControlRequestContext::new(principal, request_id)
}

fn observation_problem(error: ControlPlaneError, request_id: RequestId) -> ApiProblem {
    super::control::control_problem(error, request_id)
}

fn json_no_store<T: serde::Serialize>(body: T) -> Response {
    let mut response = (StatusCode::OK, Json(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, body::Body, http::Request};
    use tower::ServiceExt;
    use xenoteer_protocol::{WindowModelRevision, WindowQueryValidationError};

    use super::*;
    use crate::{
        AllowedOrigins, Authentication, DesktopReadiness, ReadinessHandle, ReadinessSnapshot,
        StaticCapabilityProvider, StaticTokenProvider, TransportLimits, api_router_with_planes,
        control::UnavailableControlPlane,
    };

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    struct FixtureObservation {
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        calls: Arc<AtomicUsize>,
    }

    impl ObservationPlane for FixtureObservation {
        fn list_windows<'a>(
            &'a self,
            _: ControlRequestContext,
            request: WindowListRequest,
        ) -> ObservationFuture<'a, Result<WindowListPage, ControlPlaneError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let desktop_id = self.desktop_id;
            let generation = self.generation;
            Box::pin(async move {
                if request.desktop_id != desktop_id
                    || request.desktop_generation != generation
                    || request.validate().is_err()
                {
                    return Err(ControlPlaneError::InvalidRequest);
                }
                Ok(WindowListPage {
                    desktop_id,
                    desktop_generation: generation,
                    snapshot_revision: WindowModelRevision::new(1)
                        .map_err(|_| ControlPlaneError::Internal)?,
                    windows: Vec::new(),
                    next_cursor: None,
                })
            })
        }

        fn window_snapshot<'a>(
            &'a self,
            _: ControlRequestContext,
            _: WindowSnapshotRequest,
        ) -> ObservationFuture<'a, Result<WindowSnapshotResult, ControlPlaneError>> {
            Box::pin(async { Err(ControlPlaneError::NotFound) })
        }

        fn query_windows<'a>(
            &'a self,
            _: ControlRequestContext,
            _: WindowQueryRequest,
        ) -> ObservationFuture<'a, Result<WindowQueryPage, ControlPlaneError>> {
            Box::pin(async { Err(ControlPlaneError::NotFound) })
        }

        fn resolve_window<'a>(
            &'a self,
            _: ControlRequestContext,
            _: WindowResolveRequest,
        ) -> ObservationFuture<'a, Result<WindowResolveResult, ControlPlaneError>> {
            Box::pin(async { Err(ControlPlaneError::NotFound) })
        }

        fn wait_window<'a>(
            &'a self,
            _: ControlRequestContext,
            _: WindowWaitRequest,
        ) -> ObservationFuture<'a, Result<WindowWaitResult, ControlPlaneError>> {
            Box::pin(async { Err(ControlPlaneError::NotFound) })
        }
    }

    fn application(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        principal: Principal,
        calls: Arc<AtomicUsize>,
    ) -> Result<Router, Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider = StaticTokenProvider::single(TOKEN, principal)?;
        Ok(api_router_with_planes(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
            Arc::new(UnavailableControlPlane),
            Arc::new(FixtureObservation {
                desktop_id,
                generation,
                calls,
            }),
        ))
    }

    fn list_request(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Result<Request<Body>, axum::http::Error> {
        Request::get(format!(
            "/v1/desktops/{desktop_id}/windows?desktop_generation={generation}"
        ))
        .header(
            header::AUTHORIZATION,
            "Bearer 0123456789abcdef0123456789abcdef",
        )
        .body(Body::empty())
    }

    #[tokio::test]
    async fn list_is_observer_only_generation_fenced_and_never_cached()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observer = Principal::new("observer", [Grant::DesktopObserve])?;
        let response = application(desktop_id, generation, observer, Arc::clone(&calls))?
            .oneshot(list_request(desktop_id, generation)?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("private, no-store"))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let denied_calls = Arc::new(AtomicUsize::new(0));
        let status_only = Principal::new("status", [Grant::DesktopStatus])?;
        let denied = application(
            desktop_id,
            generation,
            status_only,
            Arc::clone(&denied_calls),
        )?
        .oneshot(list_request(desktop_id, generation)?)
        .await?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(denied_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn stale_generation_is_rejected_before_actor_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observer = Principal::new("observer", [Grant::DesktopObserve])?;
        let response = application(desktop_id, generation, observer, Arc::clone(&calls))?
            .oneshot(list_request(desktop_id, DesktopGeneration::new())?)
            .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[test]
    fn fixture_page_is_a_valid_empty_atomic_snapshot() -> Result<(), WindowQueryValidationError> {
        WindowListPage {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            snapshot_revision: WindowModelRevision::new(1)?,
            windows: Vec::new(),
            next_cursor: None,
        }
        .validate()
    }
}
