//! Authenticated REST transport for leases and deduplicated commands.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use xenoteer_protocol::{
    CommandEnvelope, CommandId, CommandResult, ControlLeaseId, DesktopGeneration, DesktopId,
    EnvelopeValidationError, EventResyncReason, LeaseAcquireRequest, LeaseReleaseRequest,
    LeaseRenewRequest, LeaseStateView, LeaseValidationError, RequestId, SequencedEvent,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    problem::ApiProblem,
};

const IDEMPOTENCY_KEY: &str = "idempotency-key";
/// Long polls cannot occupy an HTTP admission slot beyond this duration.
pub const MAX_COMMAND_WAIT_MS: u32 = 30_000;

/// Boxed future used by the object-safe control-plane boundary.
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Non-secret identity and correlation data passed to control-plane adapters.
#[derive(Clone, Debug)]
pub struct ControlRequestContext {
    principal: Principal,
    request_id: RequestId,
}

impl ControlRequestContext {
    pub(crate) fn new(principal: Principal, request_id: RequestId) -> Self {
        Self {
            principal,
            request_id,
        }
    }

    /// Returns the authenticated principal.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the server-assigned transport correlation identifier.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }
}

/// Stable control-plane failures that the transport maps to RFC problem details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPlaneError {
    /// The typed request failed adapter-level validation.
    InvalidRequest,
    /// The authenticated principal cannot act on this resource.
    PermissionDenied,
    /// The requested resource does not exist or may not be disclosed.
    NotFound,
    /// The request references a previous desktop lifetime.
    StaleReference {
        /// Current generation, when one is available for resynchronization.
        current_generation: Option<DesktopGeneration>,
    },
    /// A command ID is already bound to different canonical content.
    CommandIdConflict,
    /// The requested lease transition conflicts with current lease state.
    LeaseConflict,
    /// A bounded queue or quota is currently full.
    ResourceExhausted,
    /// The desktop or required subsystem is not ready.
    CapabilityUnavailable,
    /// The target does not support the requested operation.
    UnsupportedByTarget,
    /// An atomic command stage cannot be cancelled safely.
    CancellationConflict,
    /// A private invariant failed; details must remain server-side.
    Internal,
}

/// Distinguishes new admission from idempotent retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionDisposition {
    /// A new command was admitted.
    Accepted,
    /// The exact command was already admitted and remains in progress.
    ExistingInProgress,
    /// The exact command was already terminal.
    ExistingTerminal,
}

/// Result returned by command submission.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSubmission {
    /// Current immutable command-result snapshot.
    pub result: CommandResult,
    /// Whether this request admitted work or retrieved it idempotently.
    pub disposition: SubmissionDisposition,
}

/// Result returned by cooperative cancellation.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandCancellation {
    /// Cancellation was recorded and execution is converging to terminal state.
    Accepted(CommandResult),
    /// The command was already terminal and was not changed.
    AlreadyTerminal(CommandResult),
}

/// Result of a bounded command long poll.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandWait {
    /// The command reached terminal state before the long-poll deadline.
    Terminal(CommandResult),
    /// The long poll expired; command execution continues independently.
    TimedOut(CommandResult),
}

/// Initial retained-event outcome captured atomically with a live receiver.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub enum EventReplay {
    /// A complete authoritative suffix through `latest_sequence`.
    Events {
        latest_sequence: u64,
        events: Vec<SequencedEvent>,
    },
    /// Retention or generation fencing cannot prove a complete suffix.
    ResyncRequired {
        reason: EventResyncReason,
        desktop_generation: DesktopGeneration,
        dropped_through: u64,
        latest_sequence: u64,
    },
}

/// Bounded live event-stream item returned by a control-plane adapter.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub enum LiveEvent {
    /// One globally sequenced normalized event.
    Event(SequencedEvent),
    /// The adapter lost completeness and will send no more events.
    ResyncRequired {
        reason: EventResyncReason,
        desktop_generation: DesktopGeneration,
        dropped_through: u64,
        latest_sequence: u64,
    },
    /// The coordinator closed during orderly shutdown.
    Closed,
}

/// Object-safe live half of one coordinator-owned event subscription.
pub trait LiveEventReceiver: Send + 'static {
    /// Waits for one sequenced event, explicit gap, or orderly closure.
    fn receive<'a>(&'a mut self) -> ControlFuture<'a, LiveEvent>;
}

/// Gap-free retained replay plus bounded live delivery.
pub struct EventSubscription {
    /// Retained suffix or explicit resynchronization requirement.
    pub replay: EventReplay,
    /// Bounded live receiver installed before `replay` was captured.
    pub live: Box<dyn LiveEventReceiver>,
}

/// Object-safe asynchronous seam between HTTP transport and the coordinator.
pub trait ControlPlane: Send + Sync + 'static {
    /// Returns lease state redacted for the authenticated principal.
    fn lease_state<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>>;

    /// Acquires the exclusive controller lease.
    fn acquire_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseAcquireRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>>;

    /// Explicitly renews an owned controller lease.
    fn renew_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseRenewRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>>;

    /// Releases an owned controller lease and waits for reset completion.
    fn release_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseReleaseRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>>;

    /// Admits or idempotently retrieves a typed command.
    fn submit_command<'a>(
        &'a self,
        context: ControlRequestContext,
        command: CommandEnvelope,
    ) -> ControlFuture<'a, Result<CommandSubmission, ControlPlaneError>>;

    /// Reads one principal-scoped command result.
    fn command_result<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> ControlFuture<'a, Result<CommandResult, ControlPlaneError>>;

    /// Waits without tying command execution to the transport request lifetime.
    fn wait_command<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
        timeout: Duration,
    ) -> ControlFuture<'a, Result<CommandWait, ControlPlaneError>>;

    /// Requests explicit cooperative cancellation; it never promises undo.
    fn cancel_command<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> ControlFuture<'a, Result<CommandCancellation, ControlPlaneError>>;

    /// Installs live delivery before capturing retained replay in coordinator order.
    fn subscribe_events<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        since_sequence: Option<u64>,
    ) -> ControlFuture<'a, Result<EventSubscription, ControlPlaneError>>;
}

pub(crate) type SharedControlPlane = Arc<dyn ControlPlane>;

#[derive(Debug)]
pub(crate) struct UnavailableControlPlane;

impl ControlPlane for UnavailableControlPlane {
    fn lease_state<'a>(
        &'a self,
        _: ControlRequestContext,
        _: DesktopId,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        unavailable()
    }

    fn acquire_lease<'a>(
        &'a self,
        _: ControlRequestContext,
        _: LeaseAcquireRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        unavailable()
    }

    fn renew_lease<'a>(
        &'a self,
        _: ControlRequestContext,
        _: LeaseRenewRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        unavailable()
    }

    fn release_lease<'a>(
        &'a self,
        _: ControlRequestContext,
        _: LeaseReleaseRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        unavailable()
    }

    fn submit_command<'a>(
        &'a self,
        _: ControlRequestContext,
        _: CommandEnvelope,
    ) -> ControlFuture<'a, Result<CommandSubmission, ControlPlaneError>> {
        unavailable()
    }

    fn command_result<'a>(
        &'a self,
        _: ControlRequestContext,
        _: DesktopId,
        _: CommandId,
    ) -> ControlFuture<'a, Result<CommandResult, ControlPlaneError>> {
        unavailable()
    }

    fn wait_command<'a>(
        &'a self,
        _: ControlRequestContext,
        _: DesktopId,
        _: CommandId,
        _: Duration,
    ) -> ControlFuture<'a, Result<CommandWait, ControlPlaneError>> {
        unavailable()
    }

    fn cancel_command<'a>(
        &'a self,
        _: ControlRequestContext,
        _: DesktopId,
        _: CommandId,
    ) -> ControlFuture<'a, Result<CommandCancellation, ControlPlaneError>> {
        unavailable()
    }

    fn subscribe_events<'a>(
        &'a self,
        _: ControlRequestContext,
        _: DesktopId,
        _: DesktopGeneration,
        _: Option<u64>,
    ) -> ControlFuture<'a, Result<EventSubscription, ControlPlaneError>> {
        unavailable()
    }
}

fn unavailable<'a, T>() -> ControlFuture<'a, Result<T, ControlPlaneError>> {
    Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/desktops/{desktop_id}/lease",
            get(get_lease).post(acquire_lease),
        )
        .route(
            "/v1/desktops/{desktop_id}/lease/{lease_id}/renew",
            post(renew_lease),
        )
        .route(
            "/v1/desktops/{desktop_id}/lease/{lease_id}",
            delete(release_lease),
        )
        .route("/v1/desktops/{desktop_id}/commands", post(submit_command))
        .route(
            "/v1/desktops/{desktop_id}/commands/{command_id}",
            get(get_command).delete(cancel_command),
        )
        .route(
            "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
            get(wait_command),
        )
}

async fn get_lease(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
) -> Response {
    if !principal.has_grant(Grant::InputControl) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path(desktop_id)) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_desktop_path(&state, desktop_id, request_id) {
        return problem.into_response();
    }
    let Some(generation) = state.readiness.snapshot().desktop_generation else {
        return ApiProblem::capability_unavailable(request_id).into_response();
    };
    lease_response(
        state
            .control
            .lease_state(context(principal, request_id), desktop_id)
            .await,
        desktop_id,
        generation,
        request_id,
        StatusCode::OK,
    )
}

async fn acquire_lease(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<LeaseAcquireRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::InputControl) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path(desktop_id)) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return json_rejection_response(rejection, request_id),
    };
    if let Err(problem) = validate_lease_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate(),
        request_id,
    ) {
        return problem.into_response();
    }
    let generation = request.desktop_generation;
    lease_response(
        state
            .control
            .acquire_lease(context(principal, request_id), request)
            .await,
        desktop_id,
        generation,
        request_id,
        StatusCode::CREATED,
    )
}

async fn renew_lease(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, ControlLeaseId)>, axum::extract::rejection::PathRejection>,
    body: Result<Json<LeaseRenewRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::InputControl) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path((desktop_id, lease_id))) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return json_rejection_response(rejection, request_id),
    };
    if lease_id != request.lease_id {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = validate_lease_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate(),
        request_id,
    ) {
        return problem.into_response();
    }
    let generation = request.desktop_generation;
    lease_response(
        state
            .control
            .renew_lease(context(principal, request_id), request)
            .await,
        desktop_id,
        generation,
        request_id,
        StatusCode::OK,
    )
}

async fn release_lease(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, ControlLeaseId)>, axum::extract::rejection::PathRejection>,
    body: Result<Json<LeaseReleaseRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::InputControl) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path((desktop_id, lease_id))) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return json_rejection_response(rejection, request_id),
    };
    if lease_id != request.lease_id {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = validate_lease_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate(),
        request_id,
    ) {
        return problem.into_response();
    }
    let generation = request.desktop_generation;
    lease_response(
        state
            .control
            .release_lease(context(principal, request_id), request)
            .await,
        desktop_id,
        generation,
        request_id,
        StatusCode::OK,
    )
}

async fn submit_command(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<CommandEnvelope>, JsonRejection>,
) -> Response {
    let Ok(Path(desktop_id)) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let command = match body {
        Ok(Json(command)) => command,
        Err(rejection) => return json_rejection_response(rejection, request_id),
    };
    if !principal.satisfies(crate::command_grant_requirement(&command.command)) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    if let Err(problem) = validate_command(&state, desktop_id, &command, request_id) {
        return problem.into_response();
    }
    if !idempotency_key_matches(&headers, command.command_id) {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if !state.abuse.admit_command_submit(principal.id()) {
        return ApiProblem::resource_exhausted(request_id).into_response();
    }
    match state
        .control
        .submit_command(context(principal, request_id), command)
        .await
    {
        Ok(submission) => command_submission_response(desktop_id, submission, request_id),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn json_rejection_response(rejection: JsonRejection, request_id: RequestId) -> Response {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiProblem::payload_too_large(request_id).into_response()
    } else {
        ApiProblem::invalid_request(request_id).into_response()
    }
}

async fn get_command(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, CommandId)>, axum::extract::rejection::PathRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path((desktop_id, command_id))) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_desktop_path(&state, desktop_id, request_id) {
        return problem.into_response();
    }
    match state
        .control
        .command_result(context(principal, request_id), desktop_id, command_id)
        .await
    {
        Ok(result) => command_snapshot_response(desktop_id, command_id, result, request_id),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitQuery {
    timeout_ms: Option<u32>,
}

async fn wait_command(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, CommandId)>, axum::extract::rejection::PathRejection>,
    query: Result<Query<WaitQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !principal.has_grant(Grant::DesktopObserve) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path((desktop_id, command_id))), Ok(Query(query))) = (path, query) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let timeout_ms = query.timeout_ms.unwrap_or(MAX_COMMAND_WAIT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_COMMAND_WAIT_MS {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = validate_desktop_path(&state, desktop_id, request_id) {
        return problem.into_response();
    }
    let Some(_long_poll_permit) = state.long_polls.try_acquire(principal.id()) else {
        return ApiProblem::resource_exhausted(request_id).into_response();
    };
    let transport_timeout = state.limits.request_timeout();
    let transport_margin = (transport_timeout / 10).min(Duration::from_millis(100));
    let wait_timeout = Duration::from_millis(u64::from(timeout_ms))
        .min(transport_timeout.saturating_sub(transport_margin));
    match state
        .control
        .wait_command(
            context(principal, request_id),
            desktop_id,
            command_id,
            wait_timeout,
        )
        .await
    {
        Ok(CommandWait::Terminal(result)) => {
            command_snapshot_response(desktop_id, command_id, result, request_id)
        }
        Ok(CommandWait::TimedOut(result)) => command_result_response(
            desktop_id,
            command_id,
            result,
            request_id,
            StatusCode::ACCEPTED,
            true,
        ),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

async fn cancel_command(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(DesktopId, CommandId)>, axum::extract::rejection::PathRejection>,
) -> Response {
    if !principal.has_command_cancellation_grant() {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Path((desktop_id, command_id))) = path else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_desktop_path(&state, desktop_id, request_id) {
        return problem.into_response();
    }
    match state
        .control
        .cancel_command(context(principal, request_id), desktop_id, command_id)
        .await
    {
        Ok(CommandCancellation::Accepted(result)) => command_result_response(
            desktop_id,
            command_id,
            result,
            request_id,
            StatusCode::ACCEPTED,
            true,
        ),
        Ok(CommandCancellation::AlreadyTerminal(result)) => command_result_response(
            desktop_id,
            command_id,
            result,
            request_id,
            StatusCode::OK,
            false,
        ),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn context(principal: Principal, request_id: RequestId) -> ControlRequestContext {
    ControlRequestContext::new(principal, request_id)
}

fn validate_lease_request(
    state: &ApiState,
    path_desktop: DesktopId,
    body_desktop: DesktopId,
    generation: DesktopGeneration,
    validation: Result<(), LeaseValidationError>,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    if let Err(error) = validation {
        return Err(match error {
            LeaseValidationError::UnsupportedMajor => ApiProblem::unsupported_version(request_id),
            _ => ApiProblem::invalid_request(request_id),
        });
    }
    if body_desktop != path_desktop {
        return Err(ApiProblem::invalid_request(request_id));
    }
    validate_generation(state, path_desktop, generation, request_id)
}

fn validate_command(
    state: &ApiState,
    path_desktop: DesktopId,
    command: &CommandEnvelope,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    if let Err(error) = command.validate() {
        return Err(match error {
            EnvelopeValidationError::UnsupportedMajor => {
                ApiProblem::unsupported_version(request_id)
            }
            _ => ApiProblem::invalid_request(request_id),
        });
    }
    if command.desktop_id != path_desktop {
        return Err(ApiProblem::invalid_request(request_id));
    }
    validate_generation(state, path_desktop, command.desktop_generation, request_id)
}

pub(crate) fn validate_generation(
    state: &ApiState,
    path_desktop: DesktopId,
    requested_generation: DesktopGeneration,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    validate_desktop_path(state, path_desktop, request_id)?;
    let readiness = state.readiness.snapshot();
    if !readiness.is_ready() {
        return Err(ApiProblem::capability_unavailable(request_id));
    }
    match readiness.desktop_generation {
        Some(current) if current == requested_generation => Ok(()),
        Some(current) => Err(ApiProblem::stale_reference(request_id, Some(current))),
        None => Err(ApiProblem::capability_unavailable(request_id)),
    }
}

fn validate_desktop_path(
    state: &ApiState,
    desktop_id: DesktopId,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    if desktop_id == state.desktop_id {
        Ok(())
    } else {
        Err(ApiProblem::not_found(request_id))
    }
}

fn idempotency_key_matches(headers: &HeaderMap, command_id: CommandId) -> bool {
    let mut values = headers.get_all(IDEMPOTENCY_KEY).iter();
    let Some(value) = values.next() else {
        return true;
    };
    if values.next().is_some() {
        return false;
    }
    value.as_bytes() == command_id.to_string().as_bytes()
}

fn lease_response(
    result: Result<LeaseStateView, ControlPlaneError>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    request_id: RequestId,
    status: StatusCode,
) -> Response {
    match result {
        Ok(view)
            if view.validate().is_ok()
                && view.desktop_id == desktop_id
                && view.desktop_generation == desktop_generation =>
        {
            let location = view
                .lease_id
                .map(|lease_id| format!("/v1/desktops/{desktop_id}/lease/{lease_id}"));
            json_response(status, view, location.as_deref(), false)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn command_submission_response(
    desktop_id: DesktopId,
    submission: CommandSubmission,
    request_id: RequestId,
) -> Response {
    let (status, retry) = match submission.disposition {
        SubmissionDisposition::Accepted | SubmissionDisposition::ExistingInProgress => {
            (StatusCode::ACCEPTED, true)
        }
        SubmissionDisposition::ExistingTerminal => (StatusCode::OK, false),
    };
    let command_id = submission.result.command_id();
    let lifecycle_matches = match submission.disposition {
        SubmissionDisposition::Accepted | SubmissionDisposition::ExistingInProgress => {
            !submission.result.lifecycle().is_terminal()
        }
        SubmissionDisposition::ExistingTerminal => submission.result.lifecycle().is_terminal(),
    };
    if !lifecycle_matches {
        return ApiProblem::internal(request_id).into_response();
    }
    command_result_response(
        desktop_id,
        command_id,
        submission.result,
        request_id,
        status,
        retry,
    )
}

fn command_snapshot_response(
    desktop_id: DesktopId,
    command_id: CommandId,
    result: CommandResult,
    request_id: RequestId,
) -> Response {
    let retry = !result.lifecycle().is_terminal();
    command_result_response(
        desktop_id,
        command_id,
        result,
        request_id,
        StatusCode::OK,
        retry,
    )
}

fn command_result_response(
    desktop_id: DesktopId,
    command_id: CommandId,
    result: CommandResult,
    request_id: RequestId,
    status: StatusCode,
    retry: bool,
) -> Response {
    if result.command_id() != command_id || result.validate().is_err() {
        return ApiProblem::internal(request_id).into_response();
    }
    let location = format!("/v1/desktops/{desktop_id}/commands/{command_id}");
    json_response(status, result, Some(&location), retry)
}

fn json_response<T: serde::Serialize>(
    status: StatusCode,
    body: T,
    location: Option<&str>,
    retry: bool,
) -> Response {
    let mut response = (status, Json(body)).into_response();
    if let Some(location) = location
        && let Ok(location) = HeaderValue::from_str(location)
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    if retry {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

pub(crate) fn control_problem(error: ControlPlaneError, request_id: RequestId) -> ApiProblem {
    match error {
        ControlPlaneError::InvalidRequest => ApiProblem::invalid_request(request_id),
        ControlPlaneError::PermissionDenied => ApiProblem::permission_denied(request_id),
        ControlPlaneError::NotFound => ApiProblem::not_found(request_id),
        ControlPlaneError::StaleReference { current_generation } => {
            ApiProblem::stale_reference(request_id, current_generation)
        }
        ControlPlaneError::CommandIdConflict => ApiProblem::command_id_conflict(request_id),
        ControlPlaneError::LeaseConflict => ApiProblem::lease_conflict(request_id),
        ControlPlaneError::ResourceExhausted => ApiProblem::resource_exhausted(request_id),
        ControlPlaneError::CapabilityUnavailable => ApiProblem::capability_unavailable(request_id),
        ControlPlaneError::UnsupportedByTarget => ApiProblem::unsupported_by_target(request_id),
        ControlPlaneError::CancellationConflict => ApiProblem::cancellation_conflict(request_id),
        ControlPlaneError::Internal => ApiProblem::internal(request_id),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;
    use xenoteer_protocol::{
        ApplicationId, ApplicationLaunchCommand, Command, CommandOutcome, DesktopProbeCommand,
        EffectStage, LaunchId, LeaseAvailability, Point, PointerCurve, PointerMoveCommand,
        ProcessRef, ProcessStatusCommand, ProcessTerminateCommand, ProtocolVersion, Timestamp,
    };

    use super::*;
    use crate::{
        AllowedOrigins, Authentication, DesktopReadiness, ReadinessHandle, ReadinessSnapshot,
        StaticCapabilityProvider, StaticTokenProvider, TransportLimits, api_router_with_control,
    };

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[derive(Debug, Clone, Copy)]
    enum SubmitMode {
        New,
        Duplicate,
        Terminal,
        Conflict,
    }

    #[derive(Debug)]
    struct MockControl {
        generation: DesktopGeneration,
        lease: Mutex<LeaseStateView>,
        submit: SubmitMode,
        submit_calls: AtomicUsize,
        wait_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
        adapter_calls: AtomicUsize,
    }

    impl MockControl {
        fn new(desktop_id: DesktopId, generation: DesktopGeneration, submit: SubmitMode) -> Self {
            Self {
                generation,
                lease: Mutex::new(LeaseStateView {
                    desktop_id,
                    desktop_generation: generation,
                    state: LeaseAvailability::Vacant,
                    lease_id: None,
                    expires_at: None,
                }),
                submit,
                submit_calls: AtomicUsize::new(0),
                wait_calls: AtomicUsize::new(0),
                cancel_calls: AtomicUsize::new(0),
                adapter_calls: AtomicUsize::new(0),
            }
        }

        fn result(&self, command_id: CommandId, terminal: bool) -> CommandResult {
            let at = Timestamp::parse("2026-07-21T00:00:00Z").unwrap_or_else(unreachable_timestamp);
            let accepted = CommandResult::accepted(command_id, at.clone());
            if terminal {
                accepted
                    .start(at.clone())
                    .and_then(|running| {
                        running.succeed(
                            EffectStage::PostconditionMet,
                            CommandOutcome::Acknowledged,
                            at,
                        )
                    })
                    .unwrap_or_else(unreachable_result)
            } else {
                accepted
            }
        }
    }

    fn unreachable_timestamp(error: xenoteer_protocol::TimestampError) -> Timestamp {
        unreachable!("fixed timestamp must parse: {error}")
    }

    fn unreachable_result(error: xenoteer_protocol::ResultInvariantError) -> CommandResult {
        unreachable!("fixed result transition must validate: {error}")
    }

    impl ControlPlane for MockControl {
        fn lease_state<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            let view = self
                .lease
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            Box::pin(async move { Ok(view) })
        }

        fn acquire_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            request: LeaseAcquireRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            let view = LeaseStateView {
                desktop_id: request.desktop_id,
                desktop_generation: self.generation,
                state: LeaseAvailability::HeldByCaller,
                lease_id: Some(ControlLeaseId::new()),
                expires_at: Some(
                    Timestamp::parse("2026-07-21T00:01:00Z").unwrap_or_else(unreachable_timestamp),
                ),
            };
            Box::pin(async move { Ok(view) })
        }

        fn renew_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            request: LeaseRenewRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            let view = LeaseStateView {
                desktop_id: request.desktop_id,
                desktop_generation: self.generation,
                state: LeaseAvailability::HeldByCaller,
                lease_id: Some(request.lease_id),
                expires_at: Some(
                    Timestamp::parse("2026-07-21T00:01:00Z").unwrap_or_else(unreachable_timestamp),
                ),
            };
            Box::pin(async move { Ok(view) })
        }

        fn release_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            request: LeaseReleaseRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            let view = LeaseStateView {
                desktop_id: request.desktop_id,
                desktop_generation: self.generation,
                state: LeaseAvailability::Vacant,
                lease_id: None,
                expires_at: None,
            };
            Box::pin(async move { Ok(view) })
        }

        fn submit_command<'a>(
            &'a self,
            _: ControlRequestContext,
            command: CommandEnvelope,
        ) -> ControlFuture<'a, Result<CommandSubmission, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result(
                command.command_id,
                matches!(self.submit, SubmitMode::Terminal),
            );
            let response = match self.submit {
                SubmitMode::New => Ok(CommandSubmission {
                    result,
                    disposition: SubmissionDisposition::Accepted,
                }),
                SubmitMode::Duplicate => Ok(CommandSubmission {
                    result,
                    disposition: SubmissionDisposition::ExistingInProgress,
                }),
                SubmitMode::Terminal => Ok(CommandSubmission {
                    result,
                    disposition: SubmissionDisposition::ExistingTerminal,
                }),
                SubmitMode::Conflict => Err(ControlPlaneError::CommandIdConflict),
            };
            Box::pin(async move { response })
        }

        fn command_result<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            command_id: CommandId,
        ) -> ControlFuture<'a, Result<CommandResult, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result(command_id, false);
            Box::pin(async move { Ok(result) })
        }

        fn wait_command<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            command_id: CommandId,
            _: Duration,
        ) -> ControlFuture<'a, Result<CommandWait, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result(command_id, false);
            Box::pin(async move { Ok(CommandWait::TimedOut(result)) })
        }

        fn cancel_command<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            command_id: CommandId,
        ) -> ControlFuture<'a, Result<CommandCancellation, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result(command_id, false);
            Box::pin(async move { Ok(CommandCancellation::Accepted(result)) })
        }

        fn subscribe_events<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            _: DesktopGeneration,
            _: Option<u64>,
        ) -> ControlFuture<'a, Result<EventSubscription, ControlPlaneError>> {
            self.adapter_calls.fetch_add(1, Ordering::SeqCst);
            unavailable()
        }
    }

    fn application(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        principal: Principal,
        control: Arc<MockControl>,
    ) -> Result<Router, Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider = StaticTokenProvider::single(TOKEN, principal)?;
        Ok(api_router_with_control(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
            control,
        ))
    }

    fn authorization() -> String {
        format!(
            "Bearer {}",
            std::str::from_utf8(TOKEN).unwrap_or("invalid-test-token")
        )
    }

    fn probe_envelope(desktop_id: DesktopId, generation: DesktopGeneration) -> CommandEnvelope {
        CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            Command::DesktopProbe(DesktopProbeCommand {}),
        )
        .unwrap_or_else(|error| unreachable!("fixture must validate: {error}"))
    }

    fn input_envelope(desktop_id: DesktopId, generation: DesktopGeneration) -> CommandEnvelope {
        CommandEnvelope::new_with_lease(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            ControlLeaseId::new(),
            Command::PointerMove(PointerMoveCommand {
                target: Point::new(10, 20),
                duration_ms: Some(10),
                curve: PointerCurve::Linear,
            }),
        )
        .unwrap_or_else(|error| unreachable!("fixture must validate: {error}"))
    }

    fn command_request(
        desktop_id: DesktopId,
        command: &CommandEnvelope,
    ) -> Result<axum::http::Request<Body>, Box<dyn std::error::Error>> {
        Ok(
            axum::http::Request::post(format!("/v1/desktops/{desktop_id}/commands"))
                .header(header::AUTHORIZATION, authorization())
                .header(header::CONTENT_TYPE, "application/json")
                .header(IDEMPOTENCY_KEY, command.command_id.to_string())
                .body(Body::from(serde_json::to_vec(command)?))?,
        )
    }

    fn json_authorization_request(
        method: axum::http::Method,
        uri: String,
        value: serde_json::Value,
    ) -> Result<axum::http::Request<Body>, Box<dyn std::error::Error>> {
        Ok(axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, authorization())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value)?))?)
    }

    #[derive(Debug, Clone, Copy)]
    enum AuthorizationCase {
        LeaseGet,
        LeaseAcquire,
        LeaseRenew,
        LeaseRelease,
        CommandGet,
        CommandWait,
        ProbeSubmit,
        ProcessStatusSubmit,
        InputSubmit,
        ApplicationLaunchSubmit,
        ProcessTerminateSubmit,
    }

    impl AuthorizationCase {
        const fn required_grant(self) -> Grant {
            match self {
                Self::LeaseGet
                | Self::LeaseAcquire
                | Self::LeaseRenew
                | Self::LeaseRelease
                | Self::InputSubmit => Grant::InputControl,
                Self::CommandGet
                | Self::CommandWait
                | Self::ProbeSubmit
                | Self::ProcessStatusSubmit => Grant::DesktopObserve,
                Self::ApplicationLaunchSubmit => Grant::ApplicationLaunch,
                Self::ProcessTerminateSubmit => Grant::ApplicationTerminate,
            }
        }

        const fn expected_status(self) -> StatusCode {
            match self {
                Self::LeaseAcquire => StatusCode::CREATED,
                Self::ProbeSubmit
                | Self::ProcessStatusSubmit
                | Self::InputSubmit
                | Self::ApplicationLaunchSubmit
                | Self::ProcessTerminateSubmit
                | Self::CommandWait => StatusCode::ACCEPTED,
                Self::LeaseGet | Self::LeaseRenew | Self::LeaseRelease | Self::CommandGet => {
                    StatusCode::OK
                }
            }
        }
    }

    fn process_ref(generation: DesktopGeneration) -> ProcessRef {
        ProcessRef {
            desktop_generation: generation,
            pid: 42,
            proc_start_ticks: 7,
            launch_id: LaunchId::new(),
        }
    }

    fn authorization_request(
        case: AuthorizationCase,
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Result<axum::http::Request<Body>, Box<dyn std::error::Error>> {
        let lease_id = ControlLeaseId::new();
        let command_id = CommandId::new();
        let request_id = RequestId::new();
        match case {
            AuthorizationCase::LeaseGet => Ok(axum::http::Request::get(format!(
                "/v1/desktops/{desktop_id}/lease"
            ))
            .header(header::AUTHORIZATION, authorization())
            .body(Body::empty())?),
            AuthorizationCase::LeaseAcquire => json_authorization_request(
                axum::http::Method::POST,
                format!("/v1/desktops/{desktop_id}/lease"),
                serde_json::to_value(LeaseAcquireRequest {
                    protocol_version: ProtocolVersion::V1_0,
                    request_id,
                    desktop_id,
                    desktop_generation: generation,
                    ttl_ms: Some(1_000),
                })?,
            ),
            AuthorizationCase::LeaseRenew => json_authorization_request(
                axum::http::Method::POST,
                format!("/v1/desktops/{desktop_id}/lease/{lease_id}/renew"),
                serde_json::to_value(LeaseRenewRequest {
                    protocol_version: ProtocolVersion::V1_0,
                    request_id,
                    desktop_id,
                    desktop_generation: generation,
                    lease_id,
                    ttl_ms: Some(1_000),
                })?,
            ),
            AuthorizationCase::LeaseRelease => json_authorization_request(
                axum::http::Method::DELETE,
                format!("/v1/desktops/{desktop_id}/lease/{lease_id}"),
                serde_json::to_value(LeaseReleaseRequest {
                    protocol_version: ProtocolVersion::V1_0,
                    request_id,
                    desktop_id,
                    desktop_generation: generation,
                    lease_id,
                })?,
            ),
            AuthorizationCase::CommandGet => Ok(axum::http::Request::get(format!(
                "/v1/desktops/{desktop_id}/commands/{command_id}"
            ))
            .header(header::AUTHORIZATION, authorization())
            .body(Body::empty())?),
            AuthorizationCase::CommandWait => Ok(axum::http::Request::get(format!(
                "/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=10"
            ))
            .header(header::AUTHORIZATION, authorization())
            .body(Body::empty())?),
            AuthorizationCase::ProbeSubmit
            | AuthorizationCase::ProcessStatusSubmit
            | AuthorizationCase::InputSubmit
            | AuthorizationCase::ApplicationLaunchSubmit
            | AuthorizationCase::ProcessTerminateSubmit => {
                let command = match case {
                    AuthorizationCase::ProbeSubmit => Command::DesktopProbe(DesktopProbeCommand {}),
                    AuthorizationCase::ProcessStatusSubmit => {
                        Command::ProcessStatus(ProcessStatusCommand {
                            process: process_ref(generation),
                        })
                    }
                    AuthorizationCase::InputSubmit => {
                        return command_request(
                            desktop_id,
                            &input_envelope(desktop_id, generation),
                        );
                    }
                    AuthorizationCase::ApplicationLaunchSubmit => {
                        Command::ApplicationLaunch(ApplicationLaunchCommand {
                            application: ApplicationId::new("xmessage")?,
                            arguments: Vec::new(),
                        })
                    }
                    AuthorizationCase::ProcessTerminateSubmit => {
                        Command::ProcessTerminate(ProcessTerminateCommand {
                            process: process_ref(generation),
                            grace_ms: Some(10),
                        })
                    }
                    _ => {
                        return Err(std::io::Error::other(
                            "non-submit authorization case reached submit construction",
                        )
                        .into());
                    }
                };
                let envelope = CommandEnvelope::new(
                    ProtocolVersion::V1_0,
                    request_id,
                    command_id,
                    desktop_id,
                    generation,
                    command,
                )?;
                command_request(desktop_id, &envelope)
            }
        }
    }

    fn cancellation_request(
        desktop_id: DesktopId,
    ) -> Result<axum::http::Request<Body>, Box<dyn std::error::Error>> {
        Ok(axum::http::Request::delete(format!(
            "/v1/desktops/{desktop_id}/commands/{}",
            CommandId::new()
        ))
        .header(header::AUTHORIZATION, authorization())
        .body(Body::empty())?)
    }

    #[tokio::test]
    async fn authorization_matrix_denies_before_adapter_and_accepts_minimal_grant()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            AuthorizationCase::LeaseGet,
            AuthorizationCase::LeaseAcquire,
            AuthorizationCase::LeaseRenew,
            AuthorizationCase::LeaseRelease,
            AuthorizationCase::CommandGet,
            AuthorizationCase::CommandWait,
            AuthorizationCase::ProbeSubmit,
            AuthorizationCase::ProcessStatusSubmit,
            AuthorizationCase::InputSubmit,
            AuthorizationCase::ApplicationLaunchSubmit,
            AuthorizationCase::ProcessTerminateSubmit,
        ];
        for case in cases {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let denied_control =
                Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
            let denied = application(
                desktop_id,
                generation,
                Principal::new("status-only", [Grant::DesktopStatus])?,
                Arc::clone(&denied_control),
            )?
            .oneshot(authorization_request(case, desktop_id, generation)?)
            .await?;
            assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{case:?}");
            assert_eq!(
                denied_control.adapter_calls.load(Ordering::SeqCst),
                0,
                "{case:?} reached the adapter before authorization"
            );

            let allowed_control =
                Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
            let allowed = application(
                desktop_id,
                generation,
                Principal::new("minimum-grant", [case.required_grant()])?,
                Arc::clone(&allowed_control),
            )?
            .oneshot(authorization_request(case, desktop_id, generation)?)
            .await?;
            assert_eq!(allowed.status(), case.expected_status(), "{case:?}");
            assert_eq!(
                allowed_control.adapter_calls.load(Ordering::SeqCst),
                1,
                "{case:?} did not reach the adapter exactly once"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_accepts_each_command_mutation_grant_and_rejects_non_command_grants()
    -> Result<(), Box<dyn std::error::Error>> {
        for grant in [
            Grant::InputControl,
            Grant::ApplicationLaunch,
            Grant::ApplicationTerminate,
            Grant::WindowControl,
            Grant::ClipboardWrite,
        ] {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
            let response = application(
                desktop_id,
                generation,
                Principal::new("mutation-grant", [grant])?,
                Arc::clone(&control),
            )?
            .oneshot(cancellation_request(desktop_id)?)
            .await?;
            assert_eq!(response.status(), StatusCode::ACCEPTED, "{grant:?}");
            assert_eq!(control.cancel_calls.load(Ordering::SeqCst), 1, "{grant:?}");
        }

        for grant in [
            Grant::DesktopStatus,
            Grant::DesktopObserve,
            Grant::ClipboardRead,
            Grant::CaptureRead,
            Grant::ArtifactRead,
            Grant::ArtifactDelete,
            Grant::ViewerRead,
        ] {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
            let response = application(
                desktop_id,
                generation,
                Principal::new("observation-grant", [grant])?,
                Arc::clone(&control),
            )?
            .oneshot(cancellation_request(desktop_id)?)
            .await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{grant:?}");
            assert_eq!(control.adapter_calls.load(Ordering::SeqCst), 0, "{grant:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn submit_enforces_command_specific_grant_before_adapter()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let command = input_envelope(desktop_id, generation);
        let response = application(desktop_id, generation, principal, Arc::clone(&control))?
            .oneshot(command_request(desktop_id, &command)?)
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(control.submit_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn path_body_and_idempotency_conflicts_are_rejected_before_adapter()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let command = probe_envelope(DesktopId::new(), generation);
        let path_conflict = application(
            desktop_id,
            generation,
            principal.clone(),
            Arc::clone(&control),
        )?
        .oneshot(command_request(desktop_id, &command)?)
        .await?;
        assert_eq!(path_conflict.status(), StatusCode::BAD_REQUEST);

        let command = probe_envelope(desktop_id, generation);
        let request = axum::http::Request::post(format!("/v1/desktops/{desktop_id}/commands"))
            .header(header::AUTHORIZATION, authorization())
            .header(header::CONTENT_TYPE, "application/json")
            .header(IDEMPOTENCY_KEY, CommandId::new().to_string())
            .body(Body::from(serde_json::to_vec(&command)?))?;
        let key_conflict = application(desktop_id, generation, principal, Arc::clone(&control))?
            .oneshot(request)
            .await?;
        assert_eq!(key_conflict.status(), StatusCode::BAD_REQUEST);
        assert_eq!(control.submit_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn command_conflict_and_idempotent_lifecycles_have_stable_status_headers()
    -> Result<(), Box<dyn std::error::Error>> {
        for (mode, expected) in [
            (SubmitMode::New, StatusCode::ACCEPTED),
            (SubmitMode::Duplicate, StatusCode::ACCEPTED),
            (SubmitMode::Terminal, StatusCode::OK),
            (SubmitMode::Conflict, StatusCode::CONFLICT),
        ] {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let control = Arc::new(MockControl::new(desktop_id, generation, mode));
            let principal = Principal::new("observer", [Grant::DesktopObserve])?;
            let command = probe_envelope(desktop_id, generation);
            let response = application(desktop_id, generation, principal, control)?
                .oneshot(command_request(desktop_id, &command)?)
                .await?;
            assert_eq!(response.status(), expected);
            if expected == StatusCode::ACCEPTED {
                assert_eq!(
                    response.headers().get(header::RETRY_AFTER),
                    Some(&HeaderValue::from_static("1"))
                );
                assert!(response.headers().contains_key(header::LOCATION));
            }
            if matches!(mode, SubmitMode::Conflict) {
                let body = to_bytes(response.into_body(), 4_096).await?;
                let body: serde_json::Value = serde_json::from_slice(&body)?;
                assert_eq!(body["code"], "command_id_conflict");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn long_poll_timeout_never_turns_into_implicit_cancellation()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
        let principal = Principal::new("controller", [Grant::DesktopObserve, Grant::InputControl])?;
        let response = application(desktop_id, generation, principal, Arc::clone(&control))?
            .oneshot(
                axum::http::Request::get(format!(
                    "/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=10"
                ))
                .header(header::AUTHORIZATION, authorization())
                .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(control.wait_calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.cancel_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn occupied_lease_view_never_discloses_holder_or_lease_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let control = Arc::new(MockControl::new(desktop_id, generation, SubmitMode::New));
        *control
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = LeaseStateView {
            desktop_id,
            desktop_generation: generation,
            state: LeaseAvailability::Occupied,
            lease_id: None,
            expires_at: Some(Timestamp::parse("2026-07-21T00:01:00Z")?),
        };
        let principal = Principal::new("waiting-controller", [Grant::InputControl])?;
        let response = application(desktop_id, generation, principal, control)?
            .oneshot(
                axum::http::Request::get(format!("/v1/desktops/{desktop_id}/lease"))
                    .header(header::AUTHORIZATION, authorization())
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4_096).await?;
        let text = std::str::from_utf8(&body)?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["state"], "occupied");
        assert_eq!(body["lease_id"], serde_json::Value::Null);
        assert!(!text.contains("waiting-controller"));
        Ok(())
    }
}
