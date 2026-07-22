//! Daemon composition for the coordinator, input actor, and process broker.

use std::{
    fmt::Write as _,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{Notify, broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use xenoteer_core::{
    Config,
    coordinator::{
        CancelCommandOutcome, CanonicalCommandHash, CommandEffect, CommandEventMapper,
        CommandExecutor, CommandLedgerError, CommandLedgerLimits, CommandRecord,
        CommandRecordState, CommandTerminal, CoordinatorError, CoordinatorEvent, CoordinatorHandle,
        CoordinatorSettings, EventHubError, EventHubLimits, EventRecord, ExecutionContext,
        ExecutionOutcome, ExecutionStop, GenerationToken, LeaseError, LeasePolicy,
        LeaseRequirement, LeaseSnapshot, MonotonicMillis, PrincipalId, ReplayFailure, ReplayResult,
        ResetOutcome, ResetRequest, TerminalCause, spawn_coordinator_with_event_mapper,
    },
    domain::{PointerDelta, RootPoint},
    input::{
        ButtonDirection, Effect, InputAction, LogicalButton, MotionCurve, MotionOptions,
        MotionPlanError, MotionPolicy, PhysicalButton, ScrollAction, ScrollDirection,
    },
};
use xenoteer_processd::{
    BrokerClient, BrokerClientError, BrokerErrorCode, BrokerEventReplay, BrokerLiveEvent,
    BrokerProcessEvent, DEFAULT_BROKER_SOCKET,
};
use xenoteer_protocol::{
    ACTION_LIFECYCLE_TOPIC, ArtifactRef, COMMAND_LIFECYCLE_TOPIC, ClipboardPasteEvidence,
    ClipboardRestorationEvidence, ClipboardRestorationKind, ClipboardTarget, ClipboardWriteSource,
    Command, CommandEnvelope, CommandId, CommandLifecycle, CommandOutcome, CommandResult,
    ControlLeaseId, CoordinateSpace, DesktopGeneration, DesktopId, EffectStage, ErrorCode,
    EventResyncReason, EventTopic, LeaseAcquireRequest, LeaseAvailability, LeaseReleaseRequest,
    LeaseRenewRequest, LeaseStateView, MAX_CLIPBOARD_PRESERVATION_BYTES,
    MAX_PASTE_OBSERVATION_TIMEOUT_MS, MAX_SELECTION_BYTES, MAX_TEXT_INSERT_BYTES, NormalizedEvent,
    PROCESS_EXITED_TOPIC, PointerClickTarget, PointerCurve, PointerDragTarget,
    PointerLogicalButton, PointerScrollDirection, Problem, ProcessExitedEvent, RetryAdvice,
    SelectionName, SelectionSetCommand, SelectionTransferEvidence, SelectionTransferTerminal,
    SequencedEvent, Sha256Digest, TextInsertCommand, TextInsertEvidence, TextInsertOptions,
    TextSource, TextStrategy, TextTarget, Timestamp, WindowActivateCommand, WindowActivateResult,
    WindowCloseOutcome, WindowCloseResult, WindowControlResult, WindowControlWarning,
    WindowFocusFallback, WindowGeometryRequest, WindowGeometryTarget, WindowManagerState,
    WindowMoveResizeResult, WindowMoveToWorkspaceResult,
    WindowPointerBoundsPolicy as WireWindowPointerBoundsPolicy, WindowPointerCoordinateSpace,
    WindowRect, WindowRef, WindowScreenBoundsPolicy, WindowSnapshot, WindowStackMode,
    WindowStackResult, WindowStateObservation, WindowStateOperation, WindowStateResult,
};
use xenoteer_server::{
    CommandCancellation, CommandSubmission, CommandWait, ControlFuture, ControlPlane,
    ControlPlaneError, ControlRequestContext, EventReplay, EventSubscription, Grant, LiveEvent,
    LiveEventReceiver, Principal, SubmissionDisposition, command_grant_requirement,
};
use xenoteer_x11::{
    ClipboardActorFailureKind, ClipboardActorHandle, ClipboardOwnershipEvidence,
    ClipboardOwnershipSource, ClipboardPasteObservation, ClipboardPasteObservationRequest,
    ClipboardPayload, ClipboardPayloadKind, ClipboardReadRawRequest, ClipboardReply,
    ClipboardSetRequest, ClipboardSubmitError, MAX_WINDOW_CONTROL_TIMEOUT,
    RawClipboardPasteObservation, RawClipboardReadResult, RawClipboardTarget,
    RawSelectionTransferEvidence, RawWindowControlEvidence, RawWindowControlObservation,
    RawWindowControlOperation, RawWindowControlOutcome, RawWindowControlRequest,
    RawWindowRevalidationError, WindowControlActorFailureKind, WindowControlActorHandle,
    WindowControlSubmitError,
    input::{
        ActionContext, ControlOutcome, InputActorHandle, InputEffectEvidence, InputFailure,
        InputFailureKind, InputOperation, InputOutcome, InputOutcomeKind, InputPrecondition,
        InputPreconditionFailure, InputSubmitError, KeyboardAction,
        KeyboardSequenceStep as X11KeyboardSequenceStep, MAX_PHYSICAL_TEXT_SCALARS,
        PhysicalTextMode, PointerClickRequest, PointerDragRequest, PointerEndpoint,
        PointerMoveRelativeRequest, PointerMoveRequest,
        WindowPointerBoundsPolicy as X11WindowPointerBoundsPolicy, WindowPointerClickRequest,
    },
    keyboard::{KeyIdentifier, NamedKey},
};

use crate::{
    artifact_service::{InternalArtifactContext, StoreArtifactService},
    observation_plane::DaemonObservationService,
};

const LEASE_TTL_MS: u64 = 60_000;
const MAX_CONCURRENT_EXECUTIONS: usize = 32;
const EVENT_RETENTION_COUNT: usize = 10_000;
const EVENT_RETENTION_BYTES: usize = 16 * 1024 * 1024;
const INPUT_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const WINDOW_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const WINDOW_REVALIDATION_TIMEOUT: Duration = Duration::from_secs(2);
const WINDOW_CONTROL_REPLY_BACKSTOP: Duration = Duration::from_millis(250);
const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(12);
const CLIPBOARD_READY_TIMEOUT: Duration = Duration::from_secs(3);
const PHYSICAL_TEXT_INTERVAL_MS: u16 = 5;
const PASTE_CHORD_HOLD_MS: u16 = 35;
const PROCESS_EVENT_RECONNECT_INITIAL: Duration = Duration::from_millis(50);
const PROCESS_EVENT_RECONNECT_MAXIMUM: Duration = Duration::from_secs(2);
const RESYNC_BARRIER_RETENTION_CHARGE: usize = 64;
const EXTERNAL_EVENT_QUEUE_CAPACITY: usize = 1_024;

type RuntimeHandle = CoordinatorHandle<RuntimeCommand, RuntimeResult, RuntimeEvent>;

/// Owned coordinator task and its HTTP adapter.
pub(crate) struct CoordinatorRuntime {
    handle: RuntimeHandle,
    join: JoinHandle<()>,
    process_event_cancellation: CancellationToken,
    process_event_join: JoinHandle<Result<(), ProcessEventRelayError>>,
    // Retained for Phase 4 desktop actors; the relay must remain owned even
    // before the first observation actor is wired in.
    #[allow(dead_code)]
    external_event_ingress: ExternalEventIngress,
    external_event_join: JoinHandle<Result<(), ProcessEventRelayError>>,
    control: Arc<CoordinatorControlPlane>,
}

impl CoordinatorRuntime {
    /// Returns the transport-facing object-safe adapter.
    pub(crate) fn control(&self) -> Arc<dyn ControlPlane> {
        self.control.clone()
    }

    /// Returns a handle used to fence work before HTTP graceful drain begins.
    pub(crate) fn shutdown_handle(&self) -> CoordinatorShutdownHandle {
        CoordinatorShutdownHandle {
            handle: self.handle.clone(),
            process_event_cancellation: self.process_event_cancellation.clone(),
        }
    }

    /// Returns the bounded, nonblocking event ingress used by desktop actors.
    #[allow(dead_code)]
    pub(crate) fn event_ingress(&self) -> ExternalEventIngress {
        self.external_event_ingress.clone()
    }

    /// Ensures coordinator shutdown and joins its owned task.
    pub(crate) async fn shutdown(self) -> Result<(), CoordinatorRuntimeError> {
        self.process_event_cancellation.cancel();
        match self.handle.shutdown().await {
            Ok(()) | Err(CoordinatorError::Closed) => {}
            Err(error) => return Err(error.into()),
        }
        let relay_result = self
            .process_event_join
            .await
            .map_err(|_| CoordinatorRuntimeError::ProcessEventTaskPanicked)?;
        relay_result?;
        let external_result = self
            .external_event_join
            .await
            .map_err(|_| CoordinatorRuntimeError::ExternalEventTaskPanicked)?;
        external_result?;
        self.join
            .await
            .map_err(|_| CoordinatorRuntimeError::TaskPanicked)
    }
}

/// Opaque cloneable authority used only to start coordinator drain ordering.
#[derive(Clone)]
pub(crate) struct CoordinatorShutdownHandle {
    handle: RuntimeHandle,
    process_event_cancellation: CancellationToken,
}

impl CoordinatorShutdownHandle {
    /// Stops admission, revokes/reset leases, and waits for running work.
    pub(crate) async fn shutdown(&self) -> Result<(), CoordinatorError> {
        self.process_event_cancellation.cancel();
        self.handle.shutdown().await
    }
}

/// Creates the Phase-3 coordinator over the live input-handle publication.
#[cfg(test)]
pub(crate) fn spawn(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    input: watch::Receiver<Option<InputActorHandle>>,
) -> Result<CoordinatorRuntime, CoordinatorSetupError> {
    spawn_inner(config, desktop_id, generation, input, None, None)
}

/// Creates the coordinator with the live Phase-4 window-control runtime.
#[allow(dead_code)]
pub(crate) fn spawn_with_window_control(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    input: watch::Receiver<Option<InputActorHandle>>,
    window_control: WindowControlRuntime,
) -> Result<CoordinatorRuntime, CoordinatorSetupError> {
    spawn_inner(
        config,
        desktop_id,
        generation,
        input,
        Some(window_control),
        None,
    )
}

/// Creates the coordinator with the complete Phase-4 clipboard/text runtime.
pub(crate) fn spawn_with_clipboard_runtime(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    clipboard: ClipboardRuntime,
) -> Result<CoordinatorRuntime, CoordinatorSetupError> {
    if clipboard.desktop_id != desktop_id || clipboard.generation != generation {
        return Err(CoordinatorSetupError::ClipboardScope);
    }
    spawn_inner(
        config,
        desktop_id,
        generation,
        clipboard.input.clone(),
        Some(clipboard.window_control.clone()),
        Some(clipboard),
    )
}

fn spawn_inner(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    input: watch::Receiver<Option<InputActorHandle>>,
    window_control: Option<WindowControlRuntime>,
    clipboard: Option<ClipboardRuntime>,
) -> Result<CoordinatorRuntime, CoordinatorSetupError> {
    let limits = config.limits();
    let active_global = limits.accepted_commands_per_daemon();
    let settings = CoordinatorSettings::new(
        desktop_id,
        generation,
        active_global,
        active_global,
        limits.accepted_commands_per_principal(),
        active_global.min(MAX_CONCURRENT_EXECUTIONS),
        LeasePolicy::new(LEASE_TTL_MS, LEASE_TTL_MS)?,
        CommandLedgerLimits::new(
            limits.result_ledger_entries(),
            limits
                .result_ledger_ttl_seconds()
                .checked_mul(1_000)
                .ok_or(CoordinatorSetupError::DurationOverflow)?,
        )?,
        EventHubLimits::new(EVENT_RETENTION_COUNT, EVENT_RETENTION_BYTES)?,
    )?;
    let clock = ClockProjection::capture()?;
    let broker = BrokerClient::new(DEFAULT_BROKER_SOCKET);
    let executor = RuntimeExecutor {
        input,
        window_control,
        clipboard,
        broker: broker.clone(),
        motion_policy: MotionPolicy::from_input_config(config.input())?,
        desktop_id,
        generation,
    };
    let event_mapper = RuntimeEventMapper::new()?;
    let (handle, join) = spawn_coordinator_with_event_mapper(settings, executor, event_mapper)?;
    let process_event_cancellation = CancellationToken::new();
    let (external_event_ingress, external_event_join) = spawn_external_event_relay(
        handle.clone(),
        generation,
        process_event_cancellation.clone(),
    );
    let relay_cancellation = process_event_cancellation.clone();
    let process_event_ingress = external_event_ingress.clone();
    let process_event_join = tokio::spawn(async move {
        relay_process_events(
            process_event_ingress,
            broker,
            generation,
            relay_cancellation,
        )
        .await
    });
    let control = Arc::new(CoordinatorControlPlane {
        handle: handle.clone(),
        desktop_id,
        generation,
        default_timeout: Duration::from_millis(limits.default_action_timeout_ms()),
        clock,
        #[cfg(test)]
        wait_started: None,
    });
    Ok(CoordinatorRuntime {
        handle,
        join,
        process_event_cancellation,
        process_event_join,
        external_event_ingress,
        external_event_join,
        control,
    })
}

/// Cloneable daemon composition over the dedicated raw window-control actor.
#[derive(Clone)]
pub(crate) struct WindowControlRuntime {
    actor: WindowControlActorHandle,
    observation: Arc<DaemonObservationService>,
}

impl WindowControlRuntime {
    pub(crate) fn new(
        actor: WindowControlActorHandle,
        observation: Arc<DaemonObservationService>,
    ) -> Self {
        Self { actor, observation }
    }
}

/// Cloneable daemon composition for clipboard ownership and exact text insertion.
#[derive(Clone)]
pub(crate) struct ClipboardRuntime {
    actor: ClipboardActorHandle,
    artifacts: Arc<StoreArtifactService>,
    observation: Arc<DaemonObservationService>,
    window_control: WindowControlRuntime,
    input: watch::Receiver<Option<InputActorHandle>>,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
}

impl ClipboardRuntime {
    /// Binds every authority-bearing dependency used by clipboard/text commands.
    pub(crate) fn new(
        actor: ClipboardActorHandle,
        artifacts: Arc<StoreArtifactService>,
        observation: Arc<DaemonObservationService>,
        window_control: WindowControlRuntime,
        input: watch::Receiver<Option<InputActorHandle>>,
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Self {
        Self {
            actor,
            artifacts,
            observation,
            window_control,
            input,
            desktop_id,
            generation,
        }
    }
}

type ClipboardRuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type PasteObservationFuture = Pin<
    Box<
        dyn Future<Output = Result<RawClipboardPasteObservation, ClipboardRuntimeError>>
            + Send
            + 'static,
    >,
>;

struct ArmedPasteObservation {
    wait: PasteObservationFuture,
}

#[derive(Clone, Debug)]
struct WindowInputPreconditionSpec {
    target: WindowRef,
    require_focus: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardRuntimeError {
    InvalidRequest,
    QueueFull,
    Closed,
    Operation(ClipboardActorFailureKind),
    ReplyTimedOut,
    ReplyClosed,
    BlockingTaskFailed,
}

#[derive(Debug)]
enum ClipboardInputError {
    Unavailable,
    Submit(InputSubmitError),
    Failure(InputFailure),
    ReplyClosed,
}

trait ClipboardExecutionRuntime: Send + Sync {
    fn desktop_id(&self) -> DesktopId;

    fn generation(&self) -> DesktopGeneration;

    fn read_artifact<'a>(
        &'a self,
        principal: &'a PrincipalId,
        expected: &'a ArtifactRef,
        maximum_bytes: u64,
    ) -> ClipboardRuntimeFuture<'a, Result<Vec<u8>, ControlPlaneError>>;

    fn set<'a>(
        &'a self,
        request: ClipboardSetRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>>;

    fn clear<'a>(
        &'a self,
        selection: SelectionName,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>>;

    fn read<'a>(
        &'a self,
        request: ClipboardReadRawRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<RawClipboardReadResult, ClipboardRuntimeError>>;

    fn arm_paste<'a>(
        &'a self,
        request: ClipboardPasteObservationRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ArmedPasteObservation, ClipboardRuntimeError>>;

    fn ensure_focused<'a>(
        &'a self,
        target: WindowRef,
    ) -> ClipboardRuntimeFuture<'a, Result<EffectStage, Box<RuntimeResult>>>;

    fn keyboard<'a>(
        &'a self,
        command_id: CommandId,
        deadline: Option<Instant>,
        action: KeyboardAction,
        precondition: Option<WindowInputPreconditionSpec>,
        cancellation: CancellationToken,
    ) -> ClipboardRuntimeFuture<'a, Result<InputOutcome, ClipboardInputError>>;
}

trait ClipboardExecutionContext {
    fn deadline(&self) -> Option<Instant>;

    fn stop_reason(&self) -> ExecutionStop;

    fn wait_for_stop<'a>(&'a mut self) -> ClipboardRuntimeFuture<'a, ExecutionStop>;
}

impl ClipboardExecutionContext for ExecutionContext {
    fn deadline(&self) -> Option<Instant> {
        self.deadline()
    }

    fn stop_reason(&self) -> ExecutionStop {
        self.stop_reason()
    }

    fn wait_for_stop<'a>(&'a mut self) -> ClipboardRuntimeFuture<'a, ExecutionStop> {
        Box::pin(ExecutionContext::wait_for_stop(self))
    }
}

impl ClipboardExecutionRuntime for ClipboardRuntime {
    fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    fn generation(&self) -> DesktopGeneration {
        self.generation
    }

    fn read_artifact<'a>(
        &'a self,
        principal: &'a PrincipalId,
        expected: &'a ArtifactRef,
        maximum_bytes: u64,
    ) -> ClipboardRuntimeFuture<'a, Result<Vec<u8>, ControlPlaneError>> {
        Box::pin(async move {
            let context =
                InternalArtifactContext::new(principal.as_str(), self.desktop_id, self.generation)?;
            self.artifacts
                .read_clipboard_input(&context, expected, maximum_bytes)
                .await
        })
    }

    fn set<'a>(
        &'a self,
        request: ClipboardSetRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>> {
        let reply = self
            .actor
            .try_set(request)
            .map_err(map_clipboard_submit_error);
        Box::pin(
            async move { await_clipboard_reply(reply?, deadline, CLIPBOARD_COMMAND_TIMEOUT).await },
        )
    }

    fn clear<'a>(
        &'a self,
        selection: SelectionName,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>> {
        let reply = self
            .actor
            .try_clear(selection)
            .map_err(map_clipboard_submit_error);
        Box::pin(
            async move { await_clipboard_reply(reply?, deadline, CLIPBOARD_COMMAND_TIMEOUT).await },
        )
    }

    fn read<'a>(
        &'a self,
        request: ClipboardReadRawRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<RawClipboardReadResult, ClipboardRuntimeError>> {
        let reply = self
            .actor
            .try_read(request)
            .map_err(map_clipboard_submit_error);
        Box::pin(
            async move { await_clipboard_reply(reply?, deadline, CLIPBOARD_COMMAND_TIMEOUT).await },
        )
    }

    fn arm_paste<'a>(
        &'a self,
        request: ClipboardPasteObservationRequest,
        deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ArmedPasteObservation, ClipboardRuntimeError>> {
        let observation = self
            .actor
            .try_observe_paste(request)
            .map_err(map_clipboard_submit_error);
        Box::pin(async move {
            let observation = observation?;
            let observation = await_paste_ready(observation, deadline).await?;
            Ok(ArmedPasteObservation {
                wait: Box::pin(await_paste_observation(
                    observation,
                    deadline,
                    request.timeout,
                )),
            })
        })
    }

    fn ensure_focused<'a>(
        &'a self,
        target: WindowRef,
    ) -> ClipboardRuntimeFuture<'a, Result<EffectStage, Box<RuntimeResult>>> {
        let observation = Arc::clone(&self.observation);
        let window_control = self.window_control.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let snapshot = observation
                    .snapshot_exact_blocking(target.clone(), WINDOW_REVALIDATION_TIMEOUT)
                    .map_err(|error| Box::new(map_window_model_error(error, EffectStage::None)))?;
                if snapshot.state.focused {
                    return Ok(EffectStage::None);
                }
                let activation =
                    window_control.execute(Command::WindowActivate(WindowActivateCommand {
                        window: target.clone(),
                        switch_workspace: true,
                        fallback: WindowFocusFallback::EwmhOnly,
                    }));
                let stage = activation.effect_stage();
                if matches!(activation, RuntimeResult::Failure(_)) {
                    return Err(Box::new(activation));
                }
                let focused = observation
                    .snapshot_exact_blocking(target, WINDOW_REVALIDATION_TIMEOUT)
                    .map_err(|error| Box::new(map_window_model_error(error, stage)))?;
                focused
                    .state
                    .focused
                    .then_some(stage)
                    .ok_or_else(|| Box::new(text_target_not_focused(stage)))
            })
            .await
            .unwrap_or_else(|_| Err(Box::new(backend_failure(EffectStage::OutcomeUnknown))))
        })
    }

    fn keyboard<'a>(
        &'a self,
        command_id: CommandId,
        deadline: Option<Instant>,
        action: KeyboardAction,
        precondition: Option<WindowInputPreconditionSpec>,
        cancellation: CancellationToken,
    ) -> ClipboardRuntimeFuture<'a, Result<InputOutcome, ClipboardInputError>> {
        let input = self.input.borrow().clone();
        Box::pin(async move {
            let input = input.ok_or(ClipboardInputError::Unavailable)?;
            let context = ActionContext::new(command_id, deadline.map(Instant::into_std));
            let receiver = match precondition {
                Some(precondition) => input.try_submit_keyboard_with_precondition(
                    context,
                    action,
                    exact_window_input_precondition(
                        Arc::clone(&self.observation),
                        precondition.target,
                        precondition.require_focus,
                    ),
                    cancellation,
                ),
                None => input.try_submit_keyboard(context, action, cancellation),
            }
            .map_err(ClipboardInputError::Submit)?;
            receiver
                .await
                .map_err(|_| ClipboardInputError::ReplyClosed)?
                .map_err(ClipboardInputError::Failure)
        })
    }
}

async fn await_clipboard_reply<T: Send + 'static>(
    reply: ClipboardReply<T>,
    deadline: Option<Instant>,
    maximum: Duration,
) -> Result<T, ClipboardRuntimeError> {
    let timeout = bounded_runtime_timeout(deadline, maximum)?;
    tokio::task::spawn_blocking(move || reply.recv_timeout(timeout))
        .await
        .map_err(|_| ClipboardRuntimeError::BlockingTaskFailed)?
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => ClipboardRuntimeError::ReplyTimedOut,
            std::sync::mpsc::RecvTimeoutError::Disconnected => ClipboardRuntimeError::ReplyClosed,
        })?
        .map_err(|error| ClipboardRuntimeError::Operation(error.kind))
}

async fn await_paste_ready(
    observation: ClipboardPasteObservation,
    deadline: Option<Instant>,
) -> Result<ClipboardPasteObservation, ClipboardRuntimeError> {
    let timeout = bounded_runtime_timeout(deadline, CLIPBOARD_READY_TIMEOUT)?;
    let (observation, result) = tokio::task::spawn_blocking(move || {
        let result = observation.wait_until_ready(timeout);
        (observation, result)
    })
    .await
    .map_err(|_| ClipboardRuntimeError::BlockingTaskFailed)?;
    result
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => ClipboardRuntimeError::ReplyTimedOut,
            std::sync::mpsc::RecvTimeoutError::Disconnected => ClipboardRuntimeError::ReplyClosed,
        })?
        .map_err(|error| ClipboardRuntimeError::Operation(error.kind))?;
    Ok(observation)
}

async fn await_paste_observation(
    observation: ClipboardPasteObservation,
    deadline: Option<Instant>,
    requested_timeout: Duration,
) -> Result<RawClipboardPasteObservation, ClipboardRuntimeError> {
    let maximum = requested_timeout
        .checked_add(WINDOW_CONTROL_REPLY_BACKSTOP)
        .unwrap_or(Duration::from_millis(u64::from(
            MAX_PASTE_OBSERVATION_TIMEOUT_MS,
        )));
    let timeout = bounded_runtime_timeout(deadline, maximum)?;
    tokio::task::spawn_blocking(move || observation.recv_timeout(timeout))
        .await
        .map_err(|_| ClipboardRuntimeError::BlockingTaskFailed)?
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => ClipboardRuntimeError::ReplyTimedOut,
            std::sync::mpsc::RecvTimeoutError::Disconnected => ClipboardRuntimeError::ReplyClosed,
        })?
        .map_err(|error| ClipboardRuntimeError::Operation(error.kind))
}

fn bounded_runtime_timeout(
    deadline: Option<Instant>,
    maximum: Duration,
) -> Result<Duration, ClipboardRuntimeError> {
    let remaining = deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(maximum)
        .min(maximum);
    if remaining.is_zero() {
        Err(ClipboardRuntimeError::ReplyTimedOut)
    } else {
        Ok(remaining)
    }
}

fn map_clipboard_submit_error(error: ClipboardSubmitError) -> ClipboardRuntimeError {
    match error {
        ClipboardSubmitError::InvalidRequest(_) => ClipboardRuntimeError::InvalidRequest,
        ClipboardSubmitError::QueueFull => ClipboardRuntimeError::QueueFull,
        ClipboardSubmitError::Closed => ClipboardRuntimeError::Closed,
    }
}

#[derive(Clone, Debug)]
struct RuntimeCommand {
    command_id: CommandId,
    principal: PrincipalId,
    authorization: Principal,
    command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeSuccess {
    outcome: CommandOutcome,
    effect_stage: EffectStage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeFailure {
    status: u16,
    code: ErrorCode,
    title: &'static str,
    detail: &'static str,
    retry: RetryAdvice,
    effect_stage: EffectStage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RuntimeResult {
    Success(RuntimeSuccess),
    Failure(RuntimeFailure),
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
enum RuntimeEvent {
    Targeted {
        audience: PrincipalId,
        event: NormalizedEvent,
    },
    Broadcast {
        event: NormalizedEvent,
    },
    ResyncBarrier,
}

/// Cloneable nonblocking ingress for events normalized by desktop actors.
///
/// The coordinator remains the only owner of public sequence assignment. A
/// full actor-to-daemon queue latches one resync barrier through a side channel
/// instead of blocking the X11 event loop or pretending every event survived.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct ExternalEventIngress {
    sender: mpsc::Sender<EpochRuntimeEvent>,
    resync_state: Arc<AtomicU64>,
    resync_notify: Arc<Notify>,
}

struct EpochRuntimeEvent {
    admission_epoch: u64,
    event: RuntimeEvent,
}

#[allow(dead_code)]
impl ExternalEventIngress {
    /// Publishes harmless metadata to every authenticated desktop observer.
    pub(crate) fn try_broadcast(
        &self,
        topic: EventTopic,
        payload: Value,
    ) -> Result<(), ExternalEventIngressError> {
        let event = NormalizedEvent::new(topic, payload)
            .map_err(|_| ExternalEventIngressError::InvalidEvent)?;
        self.try_send(RuntimeEvent::Broadcast { event })
    }

    /// Publishes principal-owned metadata without exposing it to other observers.
    pub(crate) fn try_targeted(
        &self,
        audience: PrincipalId,
        topic: EventTopic,
        payload: Value,
    ) -> Result<(), ExternalEventIngressError> {
        let event = NormalizedEvent::new(topic, payload)
            .map_err(|_| ExternalEventIngressError::InvalidEvent)?;
        self.try_send(RuntimeEvent::Targeted { audience, event })
    }

    /// Latches a single global resync barrier independently of queue capacity.
    pub(crate) fn require_resync(&self) {
        let mut state = self.resync_state.load(Ordering::Acquire);
        loop {
            if state & 1 == 1 {
                return;
            }
            let latched = state.wrapping_add(1);
            match self.resync_state.compare_exchange_weak(
                state,
                latched,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.resync_notify.notify_one();
                    return;
                }
                Err(observed) => state = observed,
            }
        }
    }

    fn try_require_resync(&self) -> Result<(), ExternalEventIngressError> {
        if self.sender.is_closed() {
            return Err(ExternalEventIngressError::Closed);
        }
        self.require_resync();
        if self.sender.is_closed() {
            return Err(ExternalEventIngressError::Closed);
        }
        Ok(())
    }

    fn try_send(&self, event: RuntimeEvent) -> Result<(), ExternalEventIngressError> {
        if self.sender.is_closed() {
            return Err(ExternalEventIngressError::Closed);
        }
        let admission_epoch = self.resync_state.load(Ordering::Acquire);
        // An odd epoch is a capacity-independent gap latch. An even epoch is
        // stamped onto the queue entry so admission that raced with a later
        // gap remains distinguishable after the relay claims that barrier.
        if admission_epoch & 1 == 1 {
            return if self.sender.is_closed() {
                Err(ExternalEventIngressError::Closed)
            } else {
                Err(ExternalEventIngressError::Full)
            };
        }
        match self.sender.try_send(EpochRuntimeEvent {
            admission_epoch,
            event,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.require_resync();
                Err(ExternalEventIngressError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ExternalEventIngressError::Closed),
        }
    }
}

fn claim_external_resync(resync_state: &AtomicU64) -> Option<u64> {
    let mut state = resync_state.load(Ordering::Acquire);
    loop {
        if state & 1 == 0 {
            return None;
        }
        let claimed_epoch = state.wrapping_add(1);
        match resync_state.compare_exchange_weak(
            state,
            claimed_epoch,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(claimed_epoch),
            Err(observed) => state = observed,
        }
    }
}

fn event_after_external_barrier(
    queued: EpochRuntimeEvent,
    published_epoch: u64,
) -> Option<RuntimeEvent> {
    (queued.admission_epoch == published_epoch).then_some(queued.event)
}

fn spawn_external_event_relay(
    handle: RuntimeHandle,
    generation: DesktopGeneration,
    cancellation: CancellationToken,
) -> (
    ExternalEventIngress,
    JoinHandle<Result<(), ProcessEventRelayError>>,
) {
    let (sender, receiver) = mpsc::channel(EXTERNAL_EVENT_QUEUE_CAPACITY);
    let resync_state = Arc::new(AtomicU64::new(0));
    let resync_notify = Arc::new(Notify::new());
    let ingress = ExternalEventIngress {
        sender,
        resync_state: Arc::clone(&resync_state),
        resync_notify: Arc::clone(&resync_notify),
    };
    let join = tokio::spawn(relay_external_events(
        handle,
        generation,
        cancellation,
        receiver,
        resync_state,
        resync_notify,
    ));
    (ingress, join)
}

async fn relay_external_events(
    handle: RuntimeHandle,
    desktop_generation: DesktopGeneration,
    cancellation: CancellationToken,
    mut receiver: mpsc::Receiver<EpochRuntimeEvent>,
    resync_state: Arc<AtomicU64>,
    resync_notify: Arc<Notify>,
) -> Result<(), ProcessEventRelayError> {
    let generation = handle.generation().await?;
    if generation.generation() != desktop_generation {
        return Err(ProcessEventRelayError::WrongGeneration);
    }
    let mut published_epoch = 0_u64;
    loop {
        if let Some(claimed_epoch) = claim_external_resync(&resync_state) {
            publish_resync_barrier(&handle, generation, &cancellation).await?;
            published_epoch = claimed_epoch;
            continue;
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = resync_notify.notified() => {}
            queued = receiver.recv() => match queued {
                Some(queued) => {
                    if let Some(claimed_epoch) = claim_external_resync(&resync_state) {
                        publish_resync_barrier(&handle, generation, &cancellation).await?;
                        published_epoch = claimed_epoch;
                    }
                    if let Some(event) = event_after_external_barrier(queued, published_epoch) {
                        publish_runtime_event(&handle, generation, &cancellation, event).await?;
                    }
                }
                None => return Ok(()),
            },
        }
    }
}

#[derive(Clone)]
struct RuntimeEventMapper {
    command_topic: EventTopic,
    action_topic: EventTopic,
}

impl RuntimeEventMapper {
    fn new() -> Result<Self, CoordinatorSetupError> {
        Ok(Self {
            command_topic: EventTopic::new(COMMAND_LIFECYCLE_TOPIC)
                .map_err(|_| CoordinatorSetupError::EventWire)?,
            action_topic: EventTopic::new(ACTION_LIFECYCLE_TOPIC)
                .map_err(|_| CoordinatorSetupError::EventWire)?,
        })
    }

    fn normalized(
        &self,
        principal: &PrincipalId,
        topic: EventTopic,
        payload: Value,
    ) -> Option<CoordinatorEvent<RuntimeEvent>> {
        let event = NormalizedEvent::new(topic, payload).ok()?;
        let encoded_size = event.retention_charge().ok()?;
        Some(CoordinatorEvent {
            event: RuntimeEvent::Targeted {
                audience: principal.clone(),
                event,
            },
            encoded_size,
        })
    }
}

impl CommandEventMapper<RuntimeResult, RuntimeEvent> for RuntimeEventMapper {
    fn map_command_transition(
        &self,
        principal: &PrincipalId,
        record: &CommandRecord<CommandTerminal<RuntimeResult>>,
    ) -> Vec<CoordinatorEvent<RuntimeEvent>> {
        let (lifecycle, terminal) = match &record.state {
            CommandRecordState::Accepted => ("accepted", None),
            CommandRecordState::Running => ("running", None),
            CommandRecordState::Terminal(terminal) => {
                ("terminal", Some(command_terminal_payload(terminal)))
            }
        };
        let (topic, action_state) = match &record.state {
            CommandRecordState::Accepted => (self.command_topic.clone(), None),
            CommandRecordState::Running => (self.action_topic.clone(), Some("started")),
            CommandRecordState::Terminal(_) => (self.command_topic.clone(), Some("completed")),
        };
        let payload = serde_json::json!({
            "command_id": record.command_id,
            "command_lifecycle": lifecycle,
            "action_state": action_state,
            "updated_monotonic_ms": record.updated_at.get(),
            "terminal": terminal,
        });
        self.normalized(principal, topic, payload)
            .into_iter()
            .collect()
    }
}

async fn relay_process_events(
    ingress: ExternalEventIngress,
    broker: BrokerClient,
    desktop_generation: DesktopGeneration,
    cancellation: CancellationToken,
) -> Result<(), ProcessEventRelayError> {
    let mut cursor = 0_u64;
    let mut reconnect_delay = PROCESS_EVENT_RECONNECT_INITIAL;

    loop {
        let subscription = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = broker.subscribe_events(desktop_generation, cursor) => result,
        };
        let subscription = match subscription {
            Ok(subscription) => {
                reconnect_delay = PROCESS_EVENT_RECONNECT_INITIAL;
                subscription
            }
            Err(error) => {
                tracing::warn!(error = %error, "process event stream unavailable; retrying");
                if wait_for_process_event_reconnect(&cancellation, reconnect_delay).await {
                    return Ok(());
                }
                reconnect_delay = reconnect_delay
                    .checked_mul(2)
                    .unwrap_or(PROCESS_EVENT_RECONNECT_MAXIMUM)
                    .min(PROCESS_EVENT_RECONNECT_MAXIMUM);
                continue;
            }
        };

        let xenoteer_processd::BrokerEventSubscription { replay, mut live } = subscription;
        match replay {
            BrokerEventReplay::Events {
                latest_sequence,
                events,
            } => {
                for event in events {
                    cursor = relay_process_event(
                        &ingress,
                        desktop_generation,
                        &cancellation,
                        cursor,
                        event,
                    )
                    .await?;
                }
                if cursor != latest_sequence {
                    require_process_resync(&ingress, &cancellation)?;
                    cursor = latest_sequence;
                }
            }
            BrokerEventReplay::ResyncRequired {
                latest_sequence, ..
            } => {
                require_process_resync(&ingress, &cancellation)?;
                cursor = latest_sequence;
            }
        }

        let reconnect_after_failure = loop {
            let item = tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = live.receive() => result,
            };
            match item {
                Ok(BrokerLiveEvent::Event(event)) => {
                    cursor = relay_process_event(
                        &ingress,
                        desktop_generation,
                        &cancellation,
                        cursor,
                        event,
                    )
                    .await?;
                }
                Ok(BrokerLiveEvent::ResyncRequired {
                    latest_sequence, ..
                }) => {
                    require_process_resync(&ingress, &cancellation)?;
                    cursor = latest_sequence;
                    break false;
                }
                Ok(BrokerLiveEvent::Closed) => break true,
                Err(error) => {
                    tracing::warn!(error = %error, "process event stream failed; retrying");
                    break true;
                }
            }
        };
        if reconnect_after_failure {
            if wait_for_process_event_reconnect(&cancellation, reconnect_delay).await {
                return Ok(());
            }
            reconnect_delay = reconnect_delay
                .checked_mul(2)
                .unwrap_or(PROCESS_EVENT_RECONNECT_MAXIMUM)
                .min(PROCESS_EVENT_RECONNECT_MAXIMUM);
        }
    }
}

async fn relay_process_event(
    ingress: &ExternalEventIngress,
    desktop_generation: DesktopGeneration,
    cancellation: &CancellationToken,
    cursor: u64,
    event: BrokerProcessEvent,
) -> Result<u64, ProcessEventRelayError> {
    let sequence = event.sequence();
    if sequence <= cursor {
        return Ok(cursor);
    }
    if sequence != cursor.saturating_add(1) {
        require_process_resync(ingress, cancellation)?;
    }
    match normalize_process_event(desktop_generation, event) {
        Ok(event) => publish_process_event(ingress, cancellation, event)?,
        Err(error) => {
            tracing::error!(error = %error, "invalid process event rejected");
            require_process_resync(ingress, cancellation)?;
        }
    }
    Ok(sequence)
}

fn normalize_process_event(
    desktop_generation: DesktopGeneration,
    event: BrokerProcessEvent,
) -> Result<RuntimeEvent, ProcessEventRelayError> {
    let (principal_id, payload) = event.into_parts();
    normalize_process_payload(desktop_generation, principal_id, payload)
}

fn normalize_process_payload(
    desktop_generation: DesktopGeneration,
    principal_id: String,
    payload: ProcessExitedEvent,
) -> Result<RuntimeEvent, ProcessEventRelayError> {
    if payload.process.process.desktop_generation != desktop_generation {
        return Err(ProcessEventRelayError::WrongGeneration);
    }
    payload
        .validate()
        .map_err(|_| ProcessEventRelayError::InvalidEvent)?;
    let principal =
        PrincipalId::new(principal_id).map_err(|_| ProcessEventRelayError::InvalidPrincipal)?;
    let payload =
        serde_json::to_value(payload).map_err(|_| ProcessEventRelayError::InvalidEvent)?;
    let event = NormalizedEvent::new(
        EventTopic::new(PROCESS_EXITED_TOPIC).map_err(|_| ProcessEventRelayError::InvalidEvent)?,
        payload,
    )
    .map_err(|_| ProcessEventRelayError::InvalidEvent)?;
    Ok(RuntimeEvent::Targeted {
        audience: principal,
        event,
    })
}

fn publish_process_event(
    ingress: &ExternalEventIngress,
    cancellation: &CancellationToken,
    event: RuntimeEvent,
) -> Result<(), ProcessEventRelayError> {
    match ingress.try_send(event) {
        Ok(()) | Err(ExternalEventIngressError::Full) => Ok(()),
        Err(ExternalEventIngressError::Closed) if cancellation.is_cancelled() => Ok(()),
        Err(ExternalEventIngressError::Closed) => Err(ProcessEventRelayError::EventIngressClosed),
        Err(ExternalEventIngressError::InvalidEvent) => Err(ProcessEventRelayError::InvalidEvent),
    }
}

fn require_process_resync(
    ingress: &ExternalEventIngress,
    cancellation: &CancellationToken,
) -> Result<(), ProcessEventRelayError> {
    match ingress.try_require_resync() {
        Ok(()) => Ok(()),
        Err(ExternalEventIngressError::Closed) if cancellation.is_cancelled() => Ok(()),
        Err(ExternalEventIngressError::Closed) => Err(ProcessEventRelayError::EventIngressClosed),
        Err(ExternalEventIngressError::InvalidEvent | ExternalEventIngressError::Full) => {
            Err(ProcessEventRelayError::InvalidEvent)
        }
    }
}

async fn publish_runtime_event(
    handle: &RuntimeHandle,
    generation: GenerationToken,
    cancellation: &CancellationToken,
    event: RuntimeEvent,
) -> Result<(), ProcessEventRelayError> {
    let encoded_size = match &event {
        RuntimeEvent::Targeted { event, .. } | RuntimeEvent::Broadcast { event } => event
            .retention_charge()
            .map_err(|_| ProcessEventRelayError::InvalidEvent)?,
        RuntimeEvent::ResyncBarrier => RESYNC_BARRIER_RETENTION_CHARGE,
    };
    let result = handle.publish_event(event, encoded_size, generation).await;
    match result {
        Ok(_) => Ok(()),
        Err(CoordinatorError::Closed) if cancellation.is_cancelled() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn publish_resync_barrier(
    handle: &RuntimeHandle,
    generation: GenerationToken,
    cancellation: &CancellationToken,
) -> Result<(), ProcessEventRelayError> {
    publish_runtime_event(
        handle,
        generation,
        cancellation,
        RuntimeEvent::ResyncBarrier,
    )
    .await
}

async fn wait_for_process_event_reconnect(
    cancellation: &CancellationToken,
    delay: Duration,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => true,
        () = tokio::time::sleep(delay) => false,
    }
}

fn command_terminal_payload(terminal: &CommandTerminal<RuntimeResult>) -> Value {
    serde_json::json!({
        "cause": terminal_cause_name(terminal.cause),
        "effect": match terminal.effect {
            CommandEffect::BeforeEffect => "before_effect",
            CommandEffect::AfterEffect => "after_effect",
        },
    })
}

const fn terminal_cause_name(cause: TerminalCause) -> &'static str {
    match cause {
        TerminalCause::Returned => "returned",
        TerminalCause::Cancelled => "cancelled",
        TerminalCause::DeadlineExceeded => "deadline_exceeded",
        TerminalCause::GenerationChanged => "generation_changed",
        TerminalCause::Shutdown => "shutdown",
        TerminalCause::UnexpectedStop => "unexpected_stop",
        TerminalCause::ExecutorPanicked => "executor_panicked",
    }
}

impl RuntimeResult {
    fn success(outcome: CommandOutcome, effect_stage: EffectStage) -> Self {
        Self::Success(RuntimeSuccess {
            outcome,
            effect_stage,
        })
    }

    fn failure(
        status: u16,
        code: ErrorCode,
        title: &'static str,
        detail: &'static str,
        retry: RetryAdvice,
        effect_stage: EffectStage,
    ) -> Self {
        Self::Failure(RuntimeFailure {
            status,
            code,
            title,
            detail,
            retry,
            effect_stage,
        })
    }

    const fn effect_stage(&self) -> EffectStage {
        match self {
            Self::Success(result) => result.effect_stage,
            Self::Failure(result) => result.effect_stage,
        }
    }

    fn preserve_prior_effect(mut self, prior: EffectStage) -> Self {
        if prior.has_visible_effect() && !self.effect_stage().has_visible_effect() {
            match &mut self {
                Self::Success(result) => result.effect_stage = prior,
                Self::Failure(result) => result.effect_stage = prior,
            }
        }
        self
    }
}

#[derive(Clone)]
struct RuntimeExecutor {
    input: watch::Receiver<Option<InputActorHandle>>,
    window_control: Option<WindowControlRuntime>,
    clipboard: Option<ClipboardRuntime>,
    broker: BrokerClient,
    motion_policy: MotionPolicy,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
}

impl CommandExecutor<RuntimeCommand, RuntimeResult> for RuntimeExecutor {
    fn execute(
        &self,
        command: RuntimeCommand,
        context: ExecutionContext,
    ) -> xenoteer_core::coordinator::BoxCoordinatorFuture<ExecutionOutcome<RuntimeResult>> {
        let executor = self.clone();
        Box::pin(async move { executor.execute_command(command, context).await })
    }

    fn reset_owned_input(
        &self,
        _request: ResetRequest,
    ) -> xenoteer_core::coordinator::BoxCoordinatorFuture<ResetOutcome> {
        let input = self.input.borrow().clone();
        Box::pin(async move {
            let Some(input) = input else {
                return ResetOutcome::Failed;
            };
            match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, input.reset()).await {
                Ok(Ok(Ok(ControlOutcome::Reset(evidence)))) if evidence.succeeded() => {
                    ResetOutcome::Complete
                }
                _ => ResetOutcome::Failed,
            }
        })
    }
}

impl RuntimeExecutor {
    async fn execute_command(
        &self,
        command: RuntimeCommand,
        context: ExecutionContext,
    ) -> ExecutionOutcome<RuntimeResult> {
        if command.authorization.id() != command.principal.as_str()
            || !command
                .authorization
                .satisfies(command_grant_requirement(&command.command))
        {
            return completed(permission_denied());
        }
        if context.generation().desktop_id() != self.desktop_id
            || context.generation().generation() != self.generation
        {
            return completed(stale_generation(self.generation));
        }
        let command_id = command.command_id;
        let principal = command.principal.clone();
        match command.command {
            Command::DesktopProbe(_) => completed(RuntimeResult::success(
                CommandOutcome::Probe {
                    ready: self.input.borrow().is_some(),
                },
                EffectStage::None,
            )),
            Command::PointerMove(request) => {
                let target = match RootPoint::try_from_protocol(request.target) {
                    Ok(target) => target,
                    Err(_) => return completed(invalid_input()),
                };
                let curve = match request.curve {
                    PointerCurve::Instant => MotionCurve::Instant,
                    PointerCurve::Linear => MotionCurve::Linear,
                    PointerCurve::Smooth => MotionCurve::Smooth,
                };
                let options =
                    match MotionOptions::new(curve, request.duration_ms, self.motion_policy, false)
                    {
                        Ok(options) => options,
                        Err(_) => return completed(invalid_input()),
                    };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit_pointer_move(
                    input_context(command.command_id, &context),
                    PointerMoveRequest::new(target, options),
                    cancellation.clone(),
                );
                await_input(receiver, context, cancellation, InputStage::PointerMove).await
            }
            Command::PointerMoveRelative(request) => {
                let delta = match PointerDelta::new(
                    i64::from(request.delta.x()),
                    i64::from(request.delta.y()),
                ) {
                    Ok(delta) => delta,
                    Err(_) => return completed(invalid_input()),
                };
                let options = match input_motion_options(
                    request.curve,
                    request.duration_ms,
                    self.motion_policy,
                ) {
                    Ok(options) => options,
                    Err(result) => return completed(*result),
                };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit_operation(
                    input_context(command_id, &context),
                    InputOperation::PointerMoveRelative(PointerMoveRelativeRequest::new(
                        delta, options,
                    )),
                    cancellation.clone(),
                );
                await_input(receiver, context, cancellation, InputStage::PointerMove).await
            }
            Command::PointerClick(request) => {
                execute_pointer_click_command(self, command_id, request, context).await
            }
            Command::PointerDrag(request) => {
                let endpoint = match request.target {
                    PointerDragTarget::Root { point } => {
                        match RootPoint::try_from_protocol(point) {
                            Ok(point) => PointerEndpoint::Root(point),
                            Err(_) => return completed(invalid_input()),
                        }
                    }
                    PointerDragTarget::Relative { delta } => {
                        let delta =
                            match PointerDelta::new(i64::from(delta.x()), i64::from(delta.y())) {
                                Ok(delta) => delta,
                                Err(_) => return completed(invalid_input()),
                            };
                        PointerEndpoint::Relative(delta)
                    }
                };
                let options = match input_motion_options(
                    request.curve,
                    request.duration_ms,
                    self.motion_policy,
                ) {
                    Ok(options) => options,
                    Err(result) => return completed(*result),
                };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit_operation(
                    input_context(command_id, &context),
                    InputOperation::PointerDrag(PointerDragRequest::new(
                        endpoint,
                        options,
                        input_logical_button(request.button),
                        request.press_dwell_ms,
                        request.release_dwell_ms,
                    )),
                    cancellation.clone(),
                );
                await_input(receiver, context, cancellation, InputStage::PointerDrag).await
            }
            Command::PointerScroll(request) => {
                let direction = match request.direction {
                    PointerScrollDirection::Up => ScrollDirection::Up,
                    PointerScrollDirection::Down => ScrollDirection::Down,
                    PointerScrollDirection::Left => ScrollDirection::Left,
                    PointerScrollDirection::Right => ScrollDirection::Right,
                };
                let action = match ScrollAction::new(direction, request.count, request.interval_ms)
                {
                    Ok(action) => action,
                    Err(_) => return completed(invalid_input()),
                };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit_operation(
                    input_context(command_id, &context),
                    InputOperation::PointerScroll(action),
                    cancellation.clone(),
                );
                await_input(receiver, context, cancellation, InputStage::PointerScroll).await
            }
            Command::PointerButtonDown(request) | Command::PointerButtonUp(request) => {
                let is_down = matches!(command.command, Command::PointerButtonDown(_));
                let button = match PhysicalButton::new(request.button) {
                    Ok(button) => button,
                    Err(_) => return completed(invalid_input()),
                };
                let action = InputAction::Button {
                    button,
                    direction: if is_down {
                        ButtonDirection::Down
                    } else {
                        ButtonDirection::Up
                    },
                    allow_redundant: request.allow_redundant,
                };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit(
                    input_context(command.command_id, &context),
                    action,
                    cancellation.clone(),
                );
                await_input(
                    receiver,
                    context,
                    cancellation,
                    if is_down {
                        InputStage::ButtonDown
                    } else {
                        InputStage::ButtonUp
                    },
                )
                .await
            }
            Command::KeyboardKeyDown(request) | Command::KeyboardKeyUp(request) => {
                let is_down = matches!(command.command, Command::KeyboardKeyDown(_));
                let action = if is_down {
                    KeyboardAction::down(KeyIdentifier::Raw(request.keycode))
                } else {
                    KeyboardAction::up(KeyIdentifier::Raw(request.keycode))
                };
                let action = match action {
                    Ok(action) => action,
                    Err(_) => return completed(invalid_input()),
                };
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let cancellation = CancellationToken::new();
                let receiver = input.try_submit_keyboard(
                    input_context(command.command_id, &context),
                    action,
                    cancellation.clone(),
                );
                await_input(
                    receiver,
                    context,
                    cancellation,
                    if is_down {
                        InputStage::KeyDown
                    } else {
                        InputStage::KeyUp
                    },
                )
                .await
            }
            Command::KeyboardPress(request) => {
                let action = KeyboardAction::press(request.key.into(), request.hold_ms);
                execute_keyboard_command(self, command_id, action, context).await
            }
            Command::KeyboardChord(request) => {
                let keys = request
                    .keys
                    .into_iter()
                    .map(KeyIdentifier::from)
                    .collect::<Vec<_>>();
                let action = KeyboardAction::chord(&keys, request.hold_ms);
                execute_keyboard_command(self, command_id, action, context).await
            }
            Command::KeyboardSequence(request) => {
                let steps = request
                    .steps
                    .into_iter()
                    .map(|step| {
                        let keys = step
                            .keys
                            .into_iter()
                            .map(KeyIdentifier::from)
                            .collect::<Vec<_>>();
                        X11KeyboardSequenceStep::chord(&keys, step.hold_ms, step.delay_before_ms)
                    })
                    .collect::<Result<Vec<_>, _>>();
                let action = steps.and_then(|steps| KeyboardAction::sequence(&steps));
                execute_keyboard_command(self, command_id, action, context).await
            }
            Command::InputReset(_) => {
                let Some(input) = self.input.borrow().clone() else {
                    return completed(capability_unavailable());
                };
                let response = tokio::time::timeout(INPUT_CONTROL_TIMEOUT, input.reset()).await;
                match response {
                    Ok(Ok(Ok(ControlOutcome::Reset(evidence)))) if evidence.succeeded() => {
                        let stage = if evidence.attempted() == 0 {
                            EffectStage::None
                        } else {
                            EffectStage::InputReset
                        };
                        completed(RuntimeResult::success(CommandOutcome::Acknowledged, stage))
                    }
                    Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) | Ok(Ok(Ok(_))) => {
                        completed(backend_failure(EffectStage::SideEffectObserved))
                    }
                }
            }
            Command::ApplicationLaunch(request) => {
                let broker = self.broker.clone();
                let operation_id = command.command_id;
                let principal = command.principal;
                let generation = context.generation().generation();
                await_process(
                    async move {
                        broker
                            .launch(principal.as_str(), operation_id, generation, request)
                            .await
                    },
                    context,
                    ProcessOperation::Launch,
                )
                .await
            }
            Command::ProcessStatus(request) => {
                let broker = self.broker.clone();
                await_process(
                    async move { broker.status(request.process).await },
                    context,
                    ProcessOperation::Status,
                )
                .await
            }
            Command::ProcessTerminate(request) => {
                let broker = self.broker.clone();
                await_process(
                    async move { broker.terminate_command(request).await },
                    context,
                    ProcessOperation::Terminate,
                )
                .await
            }
            command @ (Command::WindowActivate(_)
            | Command::WindowClose(_)
            | Command::WindowSetState(_)
            | Command::WindowMinimize(_)
            | Command::WindowMoveResize(_)
            | Command::WindowMoveToWorkspace(_)
            | Command::WindowStack(_)) => {
                let Some(runtime) = self.window_control.clone() else {
                    return completed(capability_unavailable());
                };
                await_window_control(runtime, command, context).await
            }
            command @ (Command::SelectionSet(_)
            | Command::SelectionClear(_)
            | Command::TextInsert(_)) => {
                let Some(runtime) = self.clipboard.clone() else {
                    return completed(capability_unavailable());
                };
                execute_clipboard_command(&runtime, command_id, principal, command, context).await
            }
        }
    }
}

async fn execute_clipboard_command<R: ClipboardExecutionRuntime + ?Sized>(
    runtime: &R,
    command_id: CommandId,
    principal: PrincipalId,
    command: Command,
    mut context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    execute_clipboard_command_with_context(runtime, command_id, principal, command, &mut context)
        .await
}

async fn execute_clipboard_command_with_context<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    command_id: CommandId,
    principal: PrincipalId,
    command: Command,
    context: &mut C,
) -> ExecutionOutcome<RuntimeResult> {
    match command {
        Command::SelectionSet(command) => {
            if command
                .validate_for_desktop(runtime.desktop_id(), runtime.generation())
                .is_err()
            {
                return completed(invalid_clipboard_request());
            }
            if !matches!(context.stop_reason(), ExecutionStop::Continue) {
                return ExecutionOutcome::Stopped {
                    effect: CommandEffect::BeforeEffect,
                };
            }
            let payload = match materialize_selection(runtime, &principal, &command).await {
                Ok(payload) => payload,
                Err(result) => return completed(result),
            };
            if !matches!(context.stop_reason(), ExecutionStop::Continue) {
                return ExecutionOutcome::Stopped {
                    effect: CommandEffect::BeforeEffect,
                };
            }
            let (result, stopped) = await_mutation_reply(
                runtime.set(
                    ClipboardSetRequest {
                        selection: command.selection,
                        payload,
                        source: ClipboardOwnershipSource::Api,
                    },
                    context.deadline(),
                ),
                context,
            )
            .await;
            finish_cancellable_mutation(
                complete_selection_mutation(result, command.selection, false),
                stopped,
            )
        }
        Command::SelectionClear(command) => {
            if !matches!(context.stop_reason(), ExecutionStop::Continue) {
                return ExecutionOutcome::Stopped {
                    effect: CommandEffect::BeforeEffect,
                };
            }
            let (result, stopped) = await_mutation_reply(
                runtime.clear(command.selection, context.deadline()),
                context,
            )
            .await;
            finish_cancellable_mutation(
                complete_selection_mutation(result, command.selection, true),
                stopped,
            )
        }
        Command::TextInsert(command) => {
            if command
                .validate_for_desktop(runtime.desktop_id(), runtime.generation())
                .is_err()
            {
                return completed(invalid_clipboard_request());
            }
            let text = match materialize_text(runtime, &principal, &command).await {
                Ok(text) => text,
                Err(result) => return completed(result),
            };
            execute_text_insert(runtime, command_id, command, text, context).await
        }
        _ => completed(invalid_clipboard_request()),
    }
}

struct MaterializedText {
    value: String,
    utf8_bytes: u64,
    unicode_scalars: u64,
}

async fn materialize_selection<R: ClipboardExecutionRuntime + ?Sized>(
    runtime: &R,
    principal: &PrincipalId,
    command: &SelectionSetCommand,
) -> Result<ClipboardPayload, RuntimeResult> {
    match &command.content {
        ClipboardWriteSource::InlineText { text } => {
            ClipboardPayload::utf8_text(text.expose_secret().to_owned())
                .map_err(|_| invalid_clipboard_request())
        }
        ClipboardWriteSource::InlineBinary { target, data } => {
            data.validate().map_err(|_| invalid_clipboard_request())?;
            let target = raw_binary_target(target).ok_or_else(invalid_clipboard_request)?;
            let bytes = STANDARD
                .decode(data.expose_base64_secret())
                .map_err(|_| invalid_clipboard_request())?;
            if bytes.len() != data.decoded_length() as usize
                || !digest_matches(&bytes, data.sha256())
            {
                return Err(invalid_clipboard_request());
            }
            ClipboardPayload::binary(target, bytes).map_err(|_| invalid_clipboard_request())
        }
        ClipboardWriteSource::Artifact { artifact, target } => {
            let bytes = runtime
                .read_artifact(principal, artifact, MAX_SELECTION_BYTES)
                .await
                .map_err(map_clipboard_artifact_error)?;
            revalidate_artifact_bytes(artifact, &bytes).map_err(|failure| *failure)?;
            if is_text_target(target) {
                if artifact.content_type.as_str() != "text/plain;charset=utf-8" {
                    return Err(invalid_clipboard_request());
                }
                let text = String::from_utf8(bytes).map_err(|_| invalid_clipboard_request())?;
                ClipboardPayload::utf8_text(text).map_err(|_| invalid_clipboard_request())
            } else {
                if artifact.content_type.as_str() != target.as_str() {
                    return Err(invalid_clipboard_request());
                }
                let target = raw_binary_target(target).ok_or_else(invalid_clipboard_request)?;
                ClipboardPayload::binary(target, bytes).map_err(|_| invalid_clipboard_request())
            }
        }
    }
}

async fn materialize_text<R: ClipboardExecutionRuntime + ?Sized>(
    runtime: &R,
    principal: &PrincipalId,
    command: &TextInsertCommand,
) -> Result<MaterializedText, RuntimeResult> {
    let value = match &command.text {
        TextSource::Inline { text } => text.expose_secret().to_owned(),
        TextSource::Artifact { artifact } => {
            if artifact.content_type.as_str() != "text/plain;charset=utf-8" {
                return Err(invalid_clipboard_request());
            }
            let bytes = runtime
                .read_artifact(principal, artifact, MAX_TEXT_INSERT_BYTES)
                .await
                .map_err(map_clipboard_artifact_error)?;
            revalidate_artifact_bytes(artifact, &bytes).map_err(|failure| *failure)?;
            String::from_utf8(bytes).map_err(|_| invalid_clipboard_request())?
        }
    };
    let utf8_bytes = u64::try_from(value.len()).map_err(|_| invalid_clipboard_request())?;
    let unicode_scalars =
        u64::try_from(value.chars().count()).map_err(|_| invalid_clipboard_request())?;
    if utf8_bytes > MAX_TEXT_INSERT_BYTES {
        return Err(invalid_clipboard_request());
    }
    Ok(MaterializedText {
        value,
        utf8_bytes,
        unicode_scalars,
    })
}

fn revalidate_artifact_bytes(
    artifact: &ArtifactRef,
    bytes: &[u8],
) -> Result<(), Box<RuntimeResult>> {
    if artifact.content_length != bytes.len() as u64 || !digest_matches(bytes, &artifact.sha256) {
        return Err(Box::new(stale_clipboard_artifact()));
    }
    Ok(())
}

fn digest_matches(bytes: &[u8], expected: &Sha256Digest) -> bool {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        if write!(&mut encoded, "{byte:02x}").is_err() {
            return false;
        }
    }
    encoded == expected.as_str()
}

fn raw_binary_target(target: &ClipboardTarget) -> Option<RawClipboardTarget> {
    match target.as_str() {
        "image/png" => Some(RawClipboardTarget::ImagePng),
        "application/octet-stream" => Some(RawClipboardTarget::ApplicationOctetStream),
        _ => None,
    }
}

fn is_text_target(target: &ClipboardTarget) -> bool {
    matches!(
        target.as_str(),
        "UTF8_STRING" | "text/plain;charset=utf-8" | "text/plain" | "STRING"
    )
}

fn complete_selection_mutation(
    result: Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>,
    expected: SelectionName,
    cleared: bool,
) -> ExecutionOutcome<RuntimeResult> {
    match result {
        Ok(evidence)
            if evidence.selection == expected
                && evidence.revision > 0
                && evidence.verified
                && ((cleared && evidence.owner == 0) || (!cleared && evidence.owner != 0)) =>
        {
            completed(RuntimeResult::success(
                CommandOutcome::Acknowledged,
                EffectStage::ClipboardOwnershipChanged,
            ))
        }
        Ok(_) => completed(backend_failure(EffectStage::OutcomeUnknown)),
        Err(error) => completed(map_clipboard_runtime_error(error, true, EffectStage::None)),
    }
}

async fn await_mutation_reply<C, F, T>(future: F, context: &mut C) -> (T, bool)
where
    C: ClipboardExecutionContext + ?Sized,
    F: Future<Output = T>,
{
    tokio::pin!(future);
    if !matches!(context.stop_reason(), ExecutionStop::Continue) {
        return (future.await, true);
    }
    tokio::select! {
        biased;
        _reason = context.wait_for_stop() => (future.await, true),
        result = &mut future => (result, false),
    }
}

fn finish_cancellable_mutation(
    outcome: ExecutionOutcome<RuntimeResult>,
    stopped: bool,
) -> ExecutionOutcome<RuntimeResult> {
    if !stopped {
        return outcome;
    }
    let effect = match outcome {
        ExecutionOutcome::Completed { effect, .. }
        | ExecutionOutcome::AtomicCompleted { effect, .. }
        | ExecutionOutcome::Stopped { effect } => effect,
    };
    ExecutionOutcome::Stopped { effect }
}

enum PhysicalAttempt {
    Terminal(ExecutionOutcome<RuntimeResult>),
    UseClipboard,
}

async fn execute_text_insert<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    command_id: CommandId,
    command: TextInsertCommand,
    text: MaterializedText,
    context: &mut C,
) -> ExecutionOutcome<RuntimeResult> {
    let target = match command.target {
        TextTarget::Window { window } => window,
    };
    match command.strategy {
        TextStrategy::Physical => {
            match execute_physical_text(
                runtime,
                command_id,
                target,
                text,
                PhysicalTextMode::CurrentLayout,
                false,
                context,
            )
            .await
            {
                PhysicalAttempt::Terminal(result) => result,
                PhysicalAttempt::UseClipboard => completed(unsupported_text_strategy()),
            }
        }
        TextStrategy::PhysicalExtended => {
            match execute_physical_text(
                runtime,
                command_id,
                target,
                text,
                PhysicalTextMode::ExtendedTemporaryMapping,
                false,
                context,
            )
            .await
            {
                PhysicalAttempt::Terminal(result) => result,
                PhysicalAttempt::UseClipboard => completed(unsupported_text_strategy()),
            }
        }
        TextStrategy::Clipboard => {
            let Some(options) = command.clipboard_options else {
                return completed(invalid_clipboard_request());
            };
            execute_clipboard_paste(runtime, command_id, target, text, options, context).await
        }
        TextStrategy::Auto => {
            let Some(options) = command.clipboard_options else {
                return completed(invalid_clipboard_request());
            };
            match execute_physical_text(
                runtime,
                command_id,
                target.clone(),
                MaterializedText {
                    value: text.value.clone(),
                    utf8_bytes: text.utf8_bytes,
                    unicode_scalars: text.unicode_scalars,
                },
                PhysicalTextMode::CurrentLayout,
                true,
                context,
            )
            .await
            {
                PhysicalAttempt::Terminal(result) => result,
                PhysicalAttempt::UseClipboard => {
                    execute_clipboard_paste(runtime, command_id, target, text, options, context)
                        .await
                }
            }
        }
    }
}

async fn execute_physical_text<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    command_id: CommandId,
    target: WindowRef,
    text: MaterializedText,
    mode: PhysicalTextMode,
    allow_clipboard_fallback: bool,
    context: &mut C,
) -> PhysicalAttempt {
    if text.unicode_scalars == 0 {
        let focus = match ensure_text_focus(runtime, target, context).await {
            Ok(stage) => stage,
            Err(result) => return PhysicalAttempt::Terminal(result),
        };
        let evidence = TextInsertEvidence {
            selected_strategy: physical_strategy(mode),
            utf8_bytes: 0,
            unicode_scalars: 0,
            completed_scalars: 0,
            clipboard: None,
        };
        if evidence.validate().is_err() {
            return PhysicalAttempt::Terminal(completed(backend_failure(focus)));
        }
        return PhysicalAttempt::Terminal(completed(RuntimeResult::success(
            CommandOutcome::TextInserted { evidence },
            focus,
        )));
    }
    if text.unicode_scalars > MAX_PHYSICAL_TEXT_SCALARS as u64 {
        return if allow_clipboard_fallback && mode == PhysicalTextMode::CurrentLayout {
            PhysicalAttempt::UseClipboard
        } else {
            PhysicalAttempt::Terminal(completed(unsupported_text_strategy()))
        };
    }
    let action = match KeyboardAction::physical_text(text.value, mode, PHYSICAL_TEXT_INTERVAL_MS) {
        Ok(action) => action,
        Err(_) => return PhysicalAttempt::Terminal(completed(unsupported_text_strategy())),
    };
    let focus_stage = match ensure_text_focus(runtime, target.clone(), context).await {
        Ok(stage) => stage,
        Err(result) => return PhysicalAttempt::Terminal(result),
    };
    let cancellation = CancellationToken::new();
    let mut operation = runtime.keyboard(
        command_id,
        context.deadline(),
        action,
        Some(WindowInputPreconditionSpec {
            target,
            require_focus: true,
        }),
        cancellation.clone(),
    );
    let result = tokio::select! {
        result = &mut operation => Some(result),
        _reason = context.wait_for_stop() => None,
    };
    let Some(result) = result else {
        cancellation.cancel();
        let effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut operation).await {
            Ok(Ok(outcome)) if outcome.events_emitted > 0 => CommandEffect::AfterEffect,
            Ok(Err(ClipboardInputError::Failure(failure)))
                if failure.events_emitted > 0 || !failure.progress_known =>
            {
                CommandEffect::AfterEffect
            }
            Err(_) | Ok(Err(ClipboardInputError::ReplyClosed)) => CommandEffect::AfterEffect,
            _ if focus_stage.has_visible_effect() => CommandEffect::AfterEffect,
            _ => CommandEffect::BeforeEffect,
        };
        return PhysicalAttempt::Terminal(ExecutionOutcome::Stopped { effect });
    };
    match result {
        Ok(outcome) => PhysicalAttempt::Terminal(complete_physical_text(
            outcome,
            command_id,
            mode,
            text.utf8_bytes,
            text.unicode_scalars,
            focus_stage,
        )),
        Err(ClipboardInputError::Failure(failure))
            if allow_clipboard_fallback
                && mode == PhysicalTextMode::CurrentLayout
                && failure.kind == InputFailureKind::TextNotRepresentable
                && failure.progress_known
                && failure.events_emitted == 0
                && failure.completed_units == 0 =>
        {
            PhysicalAttempt::UseClipboard
        }
        Err(error) => PhysicalAttempt::Terminal(completed(
            map_text_input_error(error).preserve_prior_effect(focus_stage),
        )),
    }
}

fn complete_physical_text(
    outcome: InputOutcome,
    command_id: CommandId,
    mode: PhysicalTextMode,
    utf8_bytes: u64,
    unicode_scalars: u64,
    prior_stage: EffectStage,
) -> ExecutionOutcome<RuntimeResult> {
    let stage = if outcome.events_emitted == 0 {
        prior_stage
    } else {
        EffectStage::TextInserted
    };
    if outcome.command_id != command_id || outcome.kind != InputOutcomeKind::Completed {
        return completed(map_noncompleted_text_outcome(outcome.kind, stage));
    }
    let Some(keyboard) = outcome.keyboard.as_deref() else {
        return completed(backend_failure(stage));
    };
    let completed_scalars = keyboard
        .current_layout_scalars
        .saturating_add(keyboard.temporary_mapping_scalars);
    let exact_counts = keyboard.text_scalar_count == Some(unicode_scalars as usize)
        && keyboard.requested_text_mode == Some(mode)
        && completed_scalars == unicode_scalars as usize
        && usize::from(outcome.completed_units) == completed_scalars
        && outcome.events_emitted > 0
        && matches!(
            outcome.effects,
            InputEffectEvidence::RedactedKeyboard {
                provisional: 0,
                confirmed,
            } if confirmed == outcome.events_emitted
        );
    let mapping_proof = match mode {
        PhysicalTextMode::CurrentLayout => {
            keyboard.temporary_mapping_scalars == 0
                && keyboard.temporary_mappings_installed == 0
                && keyboard.temporary_mappings_restored == 0
                && keyboard.temporary_mapping_restoration_proven.is_none()
        }
        PhysicalTextMode::ExtendedTemporaryMapping => {
            keyboard.temporary_mappings_installed == keyboard.temporary_mapping_scalars
                && keyboard.temporary_mappings_restored == keyboard.temporary_mapping_scalars
                && (keyboard.temporary_mapping_scalars == 0
                    || keyboard.temporary_mapping_restoration_proven == Some(true))
                && keyboard.temporary_mapping_restoration_proven != Some(false)
        }
    };
    if !exact_counts || !mapping_proof {
        return completed(backend_failure(if outcome.events_emitted == 0 {
            EffectStage::OutcomeUnknown
        } else {
            stage
        }));
    }
    let evidence = TextInsertEvidence {
        selected_strategy: physical_strategy(mode),
        utf8_bytes,
        unicode_scalars,
        completed_scalars: completed_scalars as u64,
        clipboard: None,
    };
    if evidence.validate().is_err() {
        return completed(backend_failure(stage));
    }
    completed(RuntimeResult::success(
        CommandOutcome::TextInserted { evidence },
        stage,
    ))
}

const fn physical_strategy(mode: PhysicalTextMode) -> TextStrategy {
    match mode {
        PhysicalTextMode::CurrentLayout => TextStrategy::Physical,
        PhysicalTextMode::ExtendedTemporaryMapping => TextStrategy::PhysicalExtended,
    }
}

async fn ensure_text_focus<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    target: WindowRef,
    context: &mut C,
) -> Result<EffectStage, ExecutionOutcome<RuntimeResult>> {
    if context.stop_reason() != ExecutionStop::Continue {
        return Err(ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        });
    }
    let result = runtime.ensure_focused(target).await;
    let effect_stage = match &result {
        Ok(stage) => *stage,
        Err(result) => result.effect_stage(),
    };
    if context.stop_reason() != ExecutionStop::Continue {
        return Err(ExecutionOutcome::Stopped {
            effect: if effect_stage.has_visible_effect() {
                CommandEffect::AfterEffect
            } else {
                CommandEffect::BeforeEffect
            },
        });
    }
    result.map_err(|result| completed(*result))
}

async fn execute_clipboard_paste<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    command_id: CommandId,
    target: WindowRef,
    text: MaterializedText,
    options: TextInsertOptions,
    context: &mut C,
) -> ExecutionOutcome<RuntimeResult> {
    if let Err(result) = ensure_text_focus(runtime, target.clone(), context).await {
        return result;
    }
    let preserved = match preserve_clipboard(runtime, options.preserve_clipboard, context).await {
        Ok(preserved) => preserved,
        Err(result) => return result,
    };
    let payload = match ClipboardPayload::utf8_text(text.value) {
        Ok(payload) => payload,
        Err(_) => return completed(invalid_clipboard_request()),
    };
    let set_result = runtime
        .set(
            ClipboardSetRequest {
                selection: SelectionName::Clipboard,
                payload,
                source: ClipboardOwnershipSource::TemporaryPaste,
            },
            context.deadline(),
        )
        .await;
    let set_verified = matches!(
        set_result,
        Ok(ClipboardOwnershipEvidence {
            selection: SelectionName::Clipboard,
            revision: 1..,
            owner: 1..,
            verified: true,
            ..
        })
    );
    if !set_verified {
        let may_have_effect = set_result
            .as_ref()
            .err()
            .is_some_and(|error| clipboard_mutation_may_have_effect(*error))
            || set_result.is_ok();
        if may_have_effect {
            let _ = restore_clipboard(runtime, &preserved).await;
        }
        let failure = match set_result {
            Ok(_) => backend_failure(EffectStage::OutcomeUnknown),
            Err(error) => map_clipboard_runtime_error(error, true, EffectStage::None),
        };
        return completed(failure);
    }
    if context.stop_reason() != ExecutionStop::Continue {
        let _ = restore_clipboard(runtime, &preserved).await;
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        };
    }

    let request = ClipboardPasteObservationRequest {
        selection: SelectionName::Clipboard,
        timeout: Duration::from_millis(u64::from(options.paste_observation_timeout_ms)),
    };
    let mut arm = runtime.arm_paste(request, context.deadline());
    let armed = tokio::select! {
        result = &mut arm => result,
        _reason = context.wait_for_stop() => {
            let _ = restore_clipboard(runtime, &preserved).await;
            return ExecutionOutcome::Stopped { effect: CommandEffect::AfterEffect };
        }
    };
    let armed = match armed {
        Ok(armed) => armed,
        Err(error) => {
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(after_clipboard_effect(map_clipboard_runtime_error(
                error,
                false,
                EffectStage::ClipboardOwnershipChanged,
            )));
        }
    };

    let focus = runtime.ensure_focused(target.clone()).await;
    if context.stop_reason() != ExecutionStop::Continue {
        let _ = restore_clipboard(runtime, &preserved).await;
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        };
    }
    if let Err(failure) = focus {
        let _ = restore_clipboard(runtime, &preserved).await;
        return completed(after_clipboard_effect(*failure));
    }

    let chord = match KeyboardAction::chord(
        &[
            KeyIdentifier::Named(NamedKey::ControlLeft),
            KeyIdentifier::Scalar('v'),
        ],
        PASTE_CHORD_HOLD_MS,
    ) {
        Ok(chord) => chord,
        Err(_) => {
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(backend_failure(EffectStage::ClipboardOwnershipChanged));
        }
    };
    let cancellation = CancellationToken::new();
    let mut keyboard = runtime.keyboard(
        command_id,
        context.deadline(),
        chord,
        Some(WindowInputPreconditionSpec {
            target,
            require_focus: true,
        }),
        cancellation.clone(),
    );
    let keyboard_result = tokio::select! {
        result = &mut keyboard => Some(result),
        _reason = context.wait_for_stop() => None,
    };
    let Some(keyboard_result) = keyboard_result else {
        cancellation.cancel();
        let _ = tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut keyboard).await;
        let _ = restore_clipboard(runtime, &preserved).await;
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        };
    };
    let input_effect = match keyboard_result {
        Ok(outcome)
            if outcome.command_id == command_id
                && outcome.kind == InputOutcomeKind::Completed
                && outcome.events_emitted >= 4
                && outcome.completed_units == 1 =>
        {
            match outcome.effects {
                InputEffectEvidence::RedactedKeyboard {
                    provisional: 0,
                    confirmed,
                } if confirmed == outcome.events_emitted => EffectStage::TextInserted,
                _ => {
                    tracing::debug!(
                        "clipboard paste rejected inconsistent keyboard effect evidence"
                    );
                    let _ = restore_clipboard(runtime, &preserved).await;
                    return completed(backend_failure(EffectStage::TextInserted));
                }
            }
        }
        Ok(outcome) => {
            tracing::debug!(
                command_matches = outcome.command_id == command_id,
                kind = ?outcome.kind,
                events_emitted = outcome.events_emitted,
                completed_units = outcome.completed_units,
                "clipboard paste keyboard outcome did not satisfy the chord contract"
            );
            let stage = if outcome.events_emitted > 0 {
                EffectStage::TextInserted
            } else {
                EffectStage::ClipboardOwnershipChanged
            };
            let failure = if outcome.kind == InputOutcomeKind::Completed {
                backend_failure(stage)
            } else {
                map_noncompleted_text_outcome(outcome.kind, stage)
            };
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(failure);
        }
        Err(error) => {
            tracing::debug!(
                error = ?error,
                "clipboard paste keyboard operation failed"
            );
            let failure = after_clipboard_effect(map_text_input_error(error));
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(failure);
        }
    };

    let mut observation = armed.wait;
    let observed = tokio::select! {
        result = &mut observation => Some(result),
        _reason = context.wait_for_stop() => None,
    };
    let Some(observed) = observed else {
        let _ = restore_clipboard(runtime, &preserved).await;
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        };
    };
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => {
            tracing::debug!(
                error = ?error,
                "clipboard paste observation failed after input"
            );
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(after_text_effect(map_clipboard_runtime_error(
                error,
                false,
                input_effect,
            )));
        }
    };
    let paste = match adapt_paste_observation(observed) {
        Ok(paste)
            if paste.request_observed && paste.transfer.as_ref().is_some_and(|e| e.completed()) =>
        {
            paste
        }
        Ok(_) => {
            tracing::debug!("clipboard paste request or terminal transfer was not observed");
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(paste_not_observed());
        }
        Err(failure) => {
            tracing::debug!("clipboard paste observation evidence was internally inconsistent");
            let _ = restore_clipboard(runtime, &preserved).await;
            return completed(*failure);
        }
    };
    let restoration = restore_clipboard(runtime, &preserved).await;
    let paste = ClipboardPasteEvidence {
        restoration,
        ..paste
    };
    let evidence = TextInsertEvidence {
        selected_strategy: TextStrategy::Clipboard,
        utf8_bytes: text.utf8_bytes,
        unicode_scalars: text.unicode_scalars,
        completed_scalars: text.unicode_scalars,
        clipboard: Some(paste),
    };
    if evidence.validate().is_err() {
        tracing::debug!("clipboard text-insert evidence failed final validation");
        return completed(backend_failure(EffectStage::TextInserted));
    }
    completed(RuntimeResult::success(
        CommandOutcome::TextInserted { evidence },
        EffectStage::TextInserted,
    ))
}

enum PreservedClipboard {
    NotRequested,
    NoOwner,
    Text {
        payload: ClipboardPayload,
        bytes: u64,
    },
}

async fn preserve_clipboard<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    requested: bool,
    context: &mut C,
) -> Result<PreservedClipboard, ExecutionOutcome<RuntimeResult>> {
    if !requested {
        return Ok(PreservedClipboard::NotRequested);
    }
    let mut read = runtime.read(
        ClipboardReadRawRequest {
            selection: SelectionName::Clipboard,
            preferred_targets: vec![
                RawClipboardTarget::Utf8String,
                RawClipboardTarget::TextPlainUtf8,
                RawClipboardTarget::TextPlain,
                RawClipboardTarget::String,
            ],
            allow_binary_fallback: false,
        },
        context.deadline(),
    );
    let result = tokio::select! {
        result = &mut read => result,
        _reason = context.wait_for_stop() => {
            return Err(ExecutionOutcome::Stopped { effect: CommandEffect::BeforeEffect });
        }
    };
    match result {
        Err(ClipboardRuntimeError::Operation(ClipboardActorFailureKind::SelectionHasNoOwner)) => {
            Ok(PreservedClipboard::NoOwner)
        }
        Err(error) => Err(completed(map_clipboard_runtime_error(
            error,
            false,
            EffectStage::None,
        ))),
        Ok(read) => validate_preserved_clipboard(read).map_err(|failure| completed(*failure)),
    }
}

fn validate_preserved_clipboard(
    read: RawClipboardReadResult,
) -> Result<PreservedClipboard, Box<RuntimeResult>> {
    let valid_target = matches!(
        read.evidence.target,
        RawClipboardTarget::Utf8String
            | RawClipboardTarget::TextPlainUtf8
            | RawClipboardTarget::TextPlain
            | RawClipboardTarget::String
    );
    let bytes = read.payload.byte_len() as u64;
    let digest = Sha256::digest(read.payload.expose_secret());
    if read.selection != SelectionName::Clipboard
        || read.revision == 0
        || read.payload.kind() != ClipboardPayloadKind::Utf8Text
        || bytes > MAX_CLIPBOARD_PRESERVATION_BYTES
        || !valid_target
        || read.evidence.content_length != bytes
        || read.evidence.sha256.as_bytes()[..] != digest[..]
        || read.evidence.owner_changed
        || !matches!(read.evidence.terminal, SelectionTransferTerminal::Completed)
        || adapt_transfer_evidence(read.evidence.clone()).is_err()
    {
        return Err(Box::new(clipboard_preservation_failed()));
    }
    Ok(PreservedClipboard::Text {
        payload: read.payload,
        bytes,
    })
}

async fn restore_clipboard<R: ClipboardExecutionRuntime + ?Sized>(
    runtime: &R,
    preserved: &PreservedClipboard,
) -> ClipboardRestorationEvidence {
    match preserved {
        PreservedClipboard::NotRequested => ClipboardRestorationEvidence {
            requested: false,
            previous_owner_existed: false,
            preserved_bytes: 0,
            kind: ClipboardRestorationKind::NotRequested,
        },
        PreservedClipboard::NoOwner => {
            let restored = matches!(
                runtime.clear(SelectionName::Clipboard, None).await,
                Ok(ClipboardOwnershipEvidence {
                    selection: SelectionName::Clipboard,
                    revision: 1..,
                    owner: 0,
                    verified: true,
                    ..
                })
            );
            ClipboardRestorationEvidence {
                requested: true,
                previous_owner_existed: false,
                preserved_bytes: 0,
                kind: if restored {
                    ClipboardRestorationKind::RelinquishedNoOwner
                } else {
                    ClipboardRestorationKind::Failed
                },
            }
        }
        PreservedClipboard::Text { payload, bytes } => {
            let restored = matches!(
                runtime
                    .set(
                        ClipboardSetRequest {
                            selection: SelectionName::Clipboard,
                            payload: payload.clone(),
                            source: ClipboardOwnershipSource::RestoredSnapshot,
                        },
                        None,
                    )
                    .await,
                Ok(ClipboardOwnershipEvidence {
                    selection: SelectionName::Clipboard,
                    revision: 1..,
                    owner: 1..,
                    verified: true,
                    ..
                })
            );
            ClipboardRestorationEvidence {
                requested: true,
                previous_owner_existed: true,
                preserved_bytes: *bytes,
                kind: if restored {
                    ClipboardRestorationKind::PartialValueCopy
                } else {
                    ClipboardRestorationKind::Failed
                },
            }
        }
    }
}

fn adapt_paste_observation(
    observed: RawClipboardPasteObservation,
) -> Result<ClipboardPasteEvidence, Box<RuntimeResult>> {
    if observed.selection != SelectionName::Clipboard {
        return Err(Box::new(backend_failure(EffectStage::TextInserted)));
    }
    let mut requested_targets = Vec::with_capacity(observed.requested_targets.len());
    for target in observed.requested_targets {
        let target = public_clipboard_target(target)
            .ok_or_else(|| Box::new(backend_failure(EffectStage::TextInserted)))?;
        requested_targets.push(target);
    }
    let transfer = observed.transfer.map(adapt_transfer_evidence).transpose()?;
    let paste = ClipboardPasteEvidence {
        request_observed: observed.request_observed,
        requested_targets,
        transfer,
        postcondition_met: None,
        restoration: ClipboardRestorationEvidence {
            requested: false,
            previous_owner_existed: false,
            preserved_bytes: 0,
            kind: ClipboardRestorationKind::NotRequested,
        },
    };
    paste
        .validate()
        .map_err(|_| Box::new(backend_failure(EffectStage::TextInserted)))?;
    Ok(paste)
}

fn adapt_transfer_evidence(
    raw: RawSelectionTransferEvidence,
) -> Result<SelectionTransferEvidence, Box<RuntimeResult>> {
    let target = public_clipboard_target(raw.target)
        .ok_or_else(|| Box::new(backend_failure(EffectStage::TextInserted)))?;
    let sha256 = sha256_from_raw(raw.sha256.as_bytes())?;
    let evidence = SelectionTransferEvidence {
        target,
        transfer: raw.transfer,
        content_length: raw.content_length,
        sha256,
        owner_changed: raw.owner_changed,
        terminal_chunk_observed: raw.terminal_chunk_observed,
        terminal: raw.terminal,
    };
    evidence
        .validate()
        .map_err(|_| Box::new(backend_failure(EffectStage::TextInserted)))?;
    Ok(evidence)
}

fn public_clipboard_target(raw: RawClipboardTarget) -> Option<ClipboardTarget> {
    if matches!(
        raw,
        RawClipboardTarget::Targets | RawClipboardTarget::Timestamp | RawClipboardTarget::Multiple
    ) {
        return None;
    }
    ClipboardTarget::new(raw.name()).ok()
}

fn sha256_from_raw(raw: &[u8; 32]) -> Result<Sha256Digest, Box<RuntimeResult>> {
    let mut encoded = String::with_capacity(64);
    for byte in raw {
        write!(&mut encoded, "{byte:02x}")
            .map_err(|_| Box::new(backend_failure(EffectStage::TextInserted)))?;
    }
    Sha256Digest::new(encoded).map_err(|_| Box::new(backend_failure(EffectStage::TextInserted)))
}

fn clipboard_mutation_may_have_effect(error: ClipboardRuntimeError) -> bool {
    matches!(
        error,
        ClipboardRuntimeError::ReplyTimedOut
            | ClipboardRuntimeError::ReplyClosed
            | ClipboardRuntimeError::BlockingTaskFailed
            | ClipboardRuntimeError::Operation(
                ClipboardActorFailureKind::OwnershipRace
                    | ClipboardActorFailureKind::OwnerChanged
                    | ClipboardActorFailureKind::BackendUnavailable
            )
    )
}

fn after_clipboard_effect(mut result: RuntimeResult) -> RuntimeResult {
    match &mut result {
        RuntimeResult::Success(success) => {
            if !success.effect_stage.has_visible_effect() {
                success.effect_stage = EffectStage::ClipboardOwnershipChanged;
            }
        }
        RuntimeResult::Failure(failure) => {
            if !failure.effect_stage.has_visible_effect() {
                failure.effect_stage = EffectStage::ClipboardOwnershipChanged;
            }
        }
    }
    result
}

fn after_text_effect(mut result: RuntimeResult) -> RuntimeResult {
    match &mut result {
        RuntimeResult::Success(success) => success.effect_stage = EffectStage::TextInserted,
        RuntimeResult::Failure(failure) => failure.effect_stage = EffectStage::TextInserted,
    }
    result
}

fn map_clipboard_artifact_error(error: ControlPlaneError) -> RuntimeResult {
    match error {
        ControlPlaneError::InvalidRequest => invalid_clipboard_request(),
        ControlPlaneError::PermissionDenied => permission_denied(),
        ControlPlaneError::NotFound => stale_clipboard_artifact(),
        ControlPlaneError::StaleReference { .. } => stale_clipboard_artifact(),
        ControlPlaneError::ResourceExhausted => clipboard_resource_exhausted(),
        ControlPlaneError::CapabilityUnavailable => capability_unavailable(),
        _ => backend_failure(EffectStage::None),
    }
}

fn map_clipboard_runtime_error(
    error: ClipboardRuntimeError,
    mutation: bool,
    prior_stage: EffectStage,
) -> RuntimeResult {
    match error {
        ClipboardRuntimeError::InvalidRequest => invalid_clipboard_request(),
        ClipboardRuntimeError::QueueFull
        | ClipboardRuntimeError::Operation(ClipboardActorFailureKind::ControlQueueFull) => {
            clipboard_resource_exhausted()
        }
        ClipboardRuntimeError::Closed
        | ClipboardRuntimeError::Operation(
            ClipboardActorFailureKind::ActorPoisoned
            | ClipboardActorFailureKind::ActorStopped
            | ClipboardActorFailureKind::ActorPanicked,
        ) => capability_unavailable(),
        ClipboardRuntimeError::ReplyTimedOut
        | ClipboardRuntimeError::ReplyClosed
        | ClipboardRuntimeError::BlockingTaskFailed
            if mutation =>
        {
            clipboard_outcome_unknown()
        }
        ClipboardRuntimeError::Operation(ClipboardActorFailureKind::SelectionTooLarge) => {
            invalid_clipboard_request()
        }
        ClipboardRuntimeError::Operation(ClipboardActorFailureKind::TargetUnsupported) => {
            unsupported_text_strategy()
        }
        ClipboardRuntimeError::Operation(ClipboardActorFailureKind::SelectionHasNoOwner) => {
            clipboard_has_no_owner()
        }
        ClipboardRuntimeError::Operation(
            ClipboardActorFailureKind::OwnershipRace
            | ClipboardActorFailureKind::OwnerChanged
            | ClipboardActorFailureKind::BackendUnavailable,
        ) if mutation => clipboard_outcome_unknown(),
        ClipboardRuntimeError::Operation(
            ClipboardActorFailureKind::TransferTimeout
            | ClipboardActorFailureKind::ProtocolViolation
            | ClipboardActorFailureKind::RequestorDestroyed
            | ClipboardActorFailureKind::OwnershipRace
            | ClipboardActorFailureKind::OwnerChanged
            | ClipboardActorFailureKind::BackendUnavailable,
        )
        | ClipboardRuntimeError::ReplyTimedOut
        | ClipboardRuntimeError::ReplyClosed
        | ClipboardRuntimeError::BlockingTaskFailed => backend_failure(prior_stage),
    }
}

fn map_text_input_error(error: ClipboardInputError) -> RuntimeResult {
    match error {
        ClipboardInputError::Unavailable
        | ClipboardInputError::Submit(InputSubmitError::Closed) => capability_unavailable(),
        ClipboardInputError::Submit(InputSubmitError::QueueFull) => resource_exhausted(),
        ClipboardInputError::ReplyClosed => backend_failure(EffectStage::OutcomeUnknown),
        ClipboardInputError::Failure(failure) => {
            let stage = if failure.events_emitted > 0 || !failure.progress_known {
                EffectStage::TextInserted
            } else {
                EffectStage::None
            };
            match failure.kind {
                InputFailureKind::CancelledBeforeEffect if stage == EffectStage::None => {
                    RuntimeResult::failure(
                        409,
                        ErrorCode::CancelledBeforeEffect,
                        "Text insertion cancelled before effect",
                        "Cancellation was observed before any text input effect.",
                        RetryAdvice::SameCommandId,
                        EffectStage::None,
                    )
                }
                InputFailureKind::DeadlineExceededBeforeEffect if stage == EffectStage::None => {
                    RuntimeResult::failure(
                        504,
                        ErrorCode::DeadlineExceededBeforeEffect,
                        "Text insertion deadline exceeded before effect",
                        "The deadline elapsed before any text input effect.",
                        RetryAdvice::SameCommandId,
                        EffectStage::None,
                    )
                }
                InputFailureKind::TextNotRepresentable
                | InputFailureKind::UnsupportedOperation
                | InputFailureKind::UnsupportedByBackend => unsupported_text_strategy_at(stage),
                InputFailureKind::HealthRejected
                | InputFailureKind::ActorStopped
                | InputFailureKind::ActorPanicked => capability_unavailable_at(stage),
                InputFailureKind::ControlQueueFull => resource_exhausted(),
                InputFailureKind::TargetStale => stale_window_reference(),
                InputFailureKind::FocusLost => text_target_not_focused(stage),
                InputFailureKind::PreconditionUnavailable => backend_failure(stage),
                _ => backend_failure(stage),
            }
        }
    }
}

fn map_noncompleted_text_outcome(
    kind: InputOutcomeKind,
    effect_stage: EffectStage,
) -> RuntimeResult {
    match kind {
        InputOutcomeKind::Completed => backend_failure(effect_stage),
        InputOutcomeKind::CancelledAfterEffect | InputOutcomeKind::DeadlineExceededAfterEffect
            if !effect_stage.has_visible_effect() =>
        {
            backend_failure(EffectStage::OutcomeUnknown)
        }
        InputOutcomeKind::CancelledAfterEffect => RuntimeResult::failure(
            409,
            ErrorCode::CancelledAfterEffect,
            "Text insertion cancelled after effect",
            "Cancellation was observed after a physical text-input effect.",
            RetryAdvice::Never,
            effect_stage,
        ),
        InputOutcomeKind::DeadlineExceededAfterEffect => RuntimeResult::failure(
            504,
            ErrorCode::DeadlineExceededAfterEffect,
            "Text insertion deadline exceeded after effect",
            "The deadline elapsed after a physical text-input effect.",
            RetryAdvice::Never,
            effect_stage,
        ),
    }
}

async fn await_window_control(
    runtime: WindowControlRuntime,
    command: Command,
    mut context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    if !matches!(context.stop_reason(), ExecutionStop::Continue) {
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        };
    }
    let stop_requested = Arc::new(AtomicBool::new(false));
    let execution_stop = Arc::clone(&stop_requested);
    let mut work =
        tokio::task::spawn_blocking(move || runtime.execute_cancellable(command, execution_stop));
    let (output, stopped) = tokio::select! {
        biased;
        _reason = context.wait_for_stop() => {
            stop_requested.store(true, Ordering::Release);
            let output = match (&mut work).await {
                Ok(output) => output,
                Err(_) => backend_failure(EffectStage::OutcomeUnknown),
            };
            (output, true)
        }
        result = &mut work => {
            let output = match result {
                Ok(output) => output,
                Err(_) => backend_failure(EffectStage::OutcomeUnknown),
            };
            (output, false)
        }
    };
    complete_window_mutation(output, stopped)
}

fn complete_window_mutation(
    output: RuntimeResult,
    stopped: bool,
) -> ExecutionOutcome<RuntimeResult> {
    finish_cancellable_mutation(completed(output), stopped)
}

impl WindowControlRuntime {
    fn execute(&self, command: Command) -> RuntimeResult {
        self.execute_cancellable(command, Arc::new(AtomicBool::new(false)))
    }

    fn execute_cancellable(
        &self,
        command: Command,
        stop_requested: Arc<AtomicBool>,
    ) -> RuntimeResult {
        if command.validate().is_err() {
            return invalid_window_control();
        }
        let (target, sibling) = match window_command_references(&command) {
            Some(references) => references,
            None => return invalid_window_control(),
        };
        let pre_effect = match self
            .observation
            .snapshot_exact_blocking(target.clone(), WINDOW_REVALIDATION_TIMEOUT)
        {
            Ok(snapshot) => snapshot,
            Err(error) => return map_window_model_error(error, EffectStage::None),
        };
        let raw_request = match prepare_raw_window_request(&command, &pre_effect) {
            Ok(request) => request,
            Err(result) => return *result,
        };
        if stop_requested.load(Ordering::Acquire) {
            return backend_failure(EffectStage::None);
        }

        let observation = Arc::clone(&self.observation);
        let revalidation_stop = Arc::clone(&stop_requested);
        let revalidate_target = target.clone();
        let revalidate_sibling = sibling.clone();
        let reply = match self.actor.try_submit(raw_request.clone(), move || {
            if revalidation_stop.load(Ordering::Acquire) {
                return Err(RawWindowRevalidationError::Rejected);
            }
            revalidate_window(&observation, revalidate_target)?;
            if let Some(sibling) = revalidate_sibling {
                revalidate_window(&observation, sibling)?;
            }
            Ok(())
        }) {
            Ok(reply) => reply,
            Err(error) => return map_window_submit_error(error),
        };
        let reply_timeout = WINDOW_CONTROL_TIMEOUT
            .checked_add(WINDOW_REVALIDATION_TIMEOUT)
            .and_then(|timeout| timeout.checked_add(WINDOW_REVALIDATION_TIMEOUT))
            .and_then(|timeout| timeout.checked_add(WINDOW_CONTROL_REPLY_BACKSTOP))
            .unwrap_or(MAX_WINDOW_CONTROL_TIMEOUT);
        let evidence = match reply.recv_timeout(reply_timeout) {
            Ok(Ok(evidence)) => evidence,
            Ok(Err(failure)) => {
                tracing::debug!(
                    failure = ?failure.kind,
                    "window-control actor request failed"
                );
                return map_window_actor_failure(failure.kind);
            }
            Err(_) => {
                tracing::debug!("window-control actor reply timed out or disconnected");
                return backend_failure(EffectStage::OutcomeUnknown);
            }
        };
        if evidence.requested != raw_request {
            tracing::debug!("window-control actor returned mismatched request evidence");
            return backend_failure(EffectStage::OutcomeUnknown);
        }
        if evidence.outcome == RawWindowControlOutcome::Unsupported {
            return unsupported_window_control();
        }
        if evidence.outcome == RawWindowControlOutcome::InvalidGeometry {
            return invalid_window_control();
        }
        if evidence.outcome == RawWindowControlOutcome::MalformedWindowManagerData {
            tracing::debug!("window-control actor rejected malformed manager evidence");
            return backend_failure(
                if matches!(evidence.observed, RawWindowControlObservation::NotObserved) {
                    EffectStage::None
                } else {
                    EffectStage::OutcomeUnknown
                },
            );
        }
        if evidence.outcome == RawWindowControlOutcome::TargetVanished
            && !matches!(&command, Command::WindowClose(_))
        {
            return backend_failure(EffectStage::OutcomeUnknown);
        }

        let raw_outcome = evidence.outcome;
        let post_effect = self
            .observation
            .snapshot_exact_blocking(target, WINDOW_REVALIDATION_TIMEOUT)
            .ok();
        let result = match translate_window_evidence(command, &pre_effect, post_effect, evidence) {
            Ok(result) => result,
            Err(stage) => {
                tracing::debug!(
                    outcome = ?raw_outcome,
                    effect_stage = ?stage,
                    "window-control evidence could not be translated"
                );
                return backend_failure(stage);
            }
        };
        let stage = window_result_stage(&result, raw_outcome);
        RuntimeResult::success(CommandOutcome::WindowControl { result }, stage)
    }
}

fn window_command_references(command: &Command) -> Option<(WindowRef, Option<WindowRef>)> {
    match command {
        Command::WindowActivate(command) => Some((command.window.clone(), None)),
        Command::WindowClose(command) => Some((command.window.clone(), None)),
        Command::WindowSetState(command) => Some((command.window.clone(), None)),
        Command::WindowMinimize(command) => Some((command.window.clone(), None)),
        Command::WindowMoveResize(command) => Some((command.window.clone(), None)),
        Command::WindowMoveToWorkspace(command) => Some((command.window.clone(), None)),
        Command::WindowStack(command) => Some((command.window.clone(), command.sibling.clone())),
        _ => None,
    }
}

fn prepare_raw_window_request(
    command: &Command,
    snapshot: &WindowSnapshot,
) -> Result<RawWindowControlRequest, Box<RuntimeResult>> {
    let operation = match command {
        Command::WindowActivate(command) => {
            let switch_workspace = if command.switch_workspace {
                Some(
                    snapshot
                        .workspace
                        .ok_or_else(|| Box::new(unsupported_window_control()))?,
                )
            } else {
                None
            };
            RawWindowControlOperation::Activate {
                timestamp: 0,
                switch_workspace,
                allow_set_input_focus: command.fallback == WindowFocusFallback::AllowSetInputFocus,
            }
        }
        Command::WindowClose(command) => RawWindowControlOperation::Close {
            timestamp: 0,
            wait_for: command.wait_for,
        },
        Command::WindowSetState(command) => RawWindowControlOperation::SetState {
            state: command.state,
            desired: command.desired,
        },
        Command::WindowMinimize(command) => RawWindowControlOperation::Minimize {
            desired: command.desired,
            timestamp: 0,
        },
        Command::WindowMoveResize(command) => RawWindowControlOperation::MoveResize {
            relative_to: command.relative_to,
            geometry: command.geometry,
            bounds_policy: command.bounds_policy,
        },
        Command::WindowMoveToWorkspace(command) => RawWindowControlOperation::MoveToWorkspace {
            workspace: command.workspace,
        },
        Command::WindowStack(command) => RawWindowControlOperation::Stack {
            mode: command.mode,
            sibling: command.sibling.as_ref().map(|window| window.xid),
            allow_raw_fallback: true,
        },
        _ => return Err(Box::new(invalid_window_control())),
    };
    Ok(RawWindowControlRequest {
        target: snapshot.window.xid,
        operation,
        timeout: WINDOW_CONTROL_TIMEOUT.min(MAX_WINDOW_CONTROL_TIMEOUT),
    })
}

fn revalidate_window(
    observation: &DaemonObservationService,
    window: WindowRef,
) -> Result<(), RawWindowRevalidationError> {
    observation
        .revalidate_exact_blocking(window, WINDOW_REVALIDATION_TIMEOUT)
        .map(|_| ())
        .map_err(|error| match error {
            ControlPlaneError::NotFound
            | ControlPlaneError::StaleReference { .. }
            | ControlPlaneError::PermissionDenied => RawWindowRevalidationError::StaleReference,
            _ => RawWindowRevalidationError::Rejected,
        })
}

fn translate_window_evidence(
    command: Command,
    pre_effect: &WindowSnapshot,
    post_effect: Option<WindowSnapshot>,
    evidence: RawWindowControlEvidence,
) -> Result<WindowControlResult, EffectStage> {
    let requested = pre_effect.window.clone();
    let result = match command {
        Command::WindowActivate(command) => {
            if evidence
                .warnings
                .contains(&WindowControlWarning::UsedSetInputFocusFallback)
                && command.fallback != WindowFocusFallback::AllowSetInputFocus
            {
                return Err(EffectStage::OutcomeUnknown);
            }
            let (observed_active, observed_focused) = match evidence.observed {
                RawWindowControlObservation::Activation {
                    active,
                    focused,
                    focus_within_target,
                    ..
                } => (
                    (active == Some(requested.xid)).then(|| requested.clone()),
                    (focused == Some(requested.xid) || focus_within_target)
                        .then(|| requested.clone()),
                ),
                _ => (None, None),
            };
            let converged = observed_active.as_ref() == Some(&requested)
                && observed_focused.as_ref() == Some(&requested);
            let mut warnings = evidence.warnings;
            set_warning(
                &mut warnings,
                WindowControlWarning::FocusNotAcquired,
                !converged,
            );
            set_warning(
                &mut warnings,
                WindowControlWarning::TargetUnmapped,
                evidence.outcome == RawWindowControlOutcome::TargetVanished,
            );
            WindowControlResult::Activated(Box::new(WindowActivateResult {
                requested,
                observed_active,
                observed_focused,
                converged,
                warnings,
            }))
        }
        Command::WindowClose(_) => {
            let raw_close = match evidence.observed {
                RawWindowControlObservation::Close { exists, viewable } => Some((exists, viewable)),
                _ => None,
            };
            let destroyed = evidence.outcome == RawWindowControlOutcome::TargetVanished
                || raw_close.is_some_and(|(exists, _)| !exists);
            let observed_unmapped = raw_close == Some((true, Some(false)));
            let (outcome, final_snapshot) =
                if evidence.outcome == RawWindowControlOutcome::RequestSent {
                    (WindowCloseOutcome::RequestSent, None)
                } else if destroyed {
                    (WindowCloseOutcome::Destroyed, None)
                } else if observed_unmapped
                    && post_effect.as_ref().is_some_and(|snapshot| {
                        snapshot.state.map_state == xenoteer_protocol::WindowMapState::Unmapped
                    })
                {
                    (WindowCloseOutcome::Unmapped, post_effect)
                } else {
                    (WindowCloseOutcome::RefusedOrTimedOut, post_effect)
                };
            let mut warnings = evidence.warnings;
            set_warning(
                &mut warnings,
                WindowControlWarning::TargetUnmapped,
                matches!(
                    outcome,
                    WindowCloseOutcome::Destroyed | WindowCloseOutcome::Unmapped
                ),
            );
            WindowControlResult::Closed(Box::new(WindowCloseResult {
                requested,
                outcome,
                final_snapshot,
                warnings,
            }))
        }
        Command::WindowSetState(command) => {
            WindowControlResult::StateChanged(Box::new(translate_state_result(
                requested,
                WindowStateOperation::ManagerState {
                    state: command.state,
                },
                command.desired,
                post_effect,
                evidence.warnings,
            )?))
        }
        Command::WindowMinimize(command) => {
            WindowControlResult::Minimized(Box::new(translate_state_result(
                requested,
                WindowStateOperation::Minimized,
                command.desired,
                post_effect,
                evidence.warnings,
            )?))
        }
        Command::WindowMoveResize(command) => WindowControlResult::GeometryChanged(Box::new(
            translate_geometry_result(requested, command, pre_effect, post_effect, evidence)?,
        )),
        Command::WindowMoveToWorkspace(command) => {
            let observed_workspace = post_effect
                .as_ref()
                .and_then(|snapshot| snapshot.workspace)
                .filter(|workspace| *workspace != u32::MAX);
            let converged = observed_workspace == Some(command.workspace);
            let mut warnings = evidence.warnings;
            set_warning(
                &mut warnings,
                WindowControlWarning::WorkspaceNotConfirmed,
                !converged,
            );
            set_warning(
                &mut warnings,
                WindowControlWarning::TargetUnmapped,
                evidence.outcome == RawWindowControlOutcome::TargetVanished,
            );
            WindowControlResult::WorkspaceChanged(Box::new(WindowMoveToWorkspaceResult {
                requested,
                desired_workspace: command.workspace,
                observed_workspace,
                observed_revision: post_effect
                    .as_ref()
                    .map_or(pre_effect.model_revision, |snapshot| {
                        snapshot.model_revision
                    }),
                converged,
                warnings,
            }))
        }
        Command::WindowStack(command) => {
            let (target_index, sibling_index) = match evidence.observed {
                RawWindowControlObservation::Stacking {
                    target_index,
                    sibling_index,
                    ..
                } => (target_index, sibling_index),
                _ => (None, None),
            };
            let converged = match command.mode {
                WindowStackMode::Above => target_index
                    .zip(sibling_index)
                    .is_some_and(|(target, sibling)| target > sibling),
                WindowStackMode::Below => target_index
                    .zip(sibling_index)
                    .is_some_and(|(target, sibling)| target < sibling),
                WindowStackMode::Raise | WindowStackMode::Lower => {
                    evidence.outcome == RawWindowControlOutcome::Converged
                }
            };
            let mut warnings = evidence.warnings;
            set_warning(
                &mut warnings,
                WindowControlWarning::StackingNotConfirmed,
                !converged,
            );
            set_warning(
                &mut warnings,
                WindowControlWarning::TargetUnmapped,
                evidence.outcome == RawWindowControlOutcome::TargetVanished,
            );
            WindowControlResult::Stacked(Box::new(WindowStackResult {
                requested,
                mode: command.mode,
                sibling: command.sibling,
                observed_stacking_index: target_index,
                observed_sibling_index: sibling_index,
                observed_revision: post_effect
                    .as_ref()
                    .map_or(pre_effect.model_revision, |snapshot| {
                        snapshot.model_revision
                    }),
                converged,
                warnings,
            }))
        }
        _ => return Err(EffectStage::None),
    };
    result.validate().map_err(|_| EffectStage::OutcomeUnknown)?;
    Ok(result)
}

fn translate_state_result(
    requested: WindowRef,
    operation: WindowStateOperation,
    desired: bool,
    final_snapshot: Option<WindowSnapshot>,
    mut warnings: Vec<WindowControlWarning>,
) -> Result<WindowStateResult, EffectStage> {
    let final_snapshot = final_snapshot.ok_or(EffectStage::OutcomeUnknown)?;
    let observed = observe_public_window_state(operation, &final_snapshot);
    let converged = matches!(
        (observed, desired),
        (WindowStateObservation::Enabled, true) | (WindowStateObservation::Disabled, false)
    );
    set_warning(
        &mut warnings,
        WindowControlWarning::PartialStateObserved,
        observed == WindowStateObservation::Partial,
    );
    Ok(WindowStateResult {
        requested,
        operation,
        desired,
        observed,
        converged,
        final_snapshot,
        warnings,
    })
}

fn observe_public_window_state(
    operation: WindowStateOperation,
    snapshot: &WindowSnapshot,
) -> WindowStateObservation {
    let has_atom = |atom: &str| {
        snapshot
            .metadata
            .states
            .iter()
            .any(|observed| observed.as_str() == atom)
    };
    let enabled = |value| {
        if value {
            WindowStateObservation::Enabled
        } else {
            WindowStateObservation::Disabled
        }
    };
    match operation {
        WindowStateOperation::Minimized => enabled(snapshot.state.minimized),
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Maximized,
        } => match (
            has_atom("_NET_WM_STATE_MAXIMIZED_VERT"),
            has_atom("_NET_WM_STATE_MAXIMIZED_HORZ"),
        ) {
            (true, true) => WindowStateObservation::Enabled,
            (false, false) => WindowStateObservation::Disabled,
            _ => WindowStateObservation::Partial,
        },
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Fullscreen,
        } => enabled(has_atom("_NET_WM_STATE_FULLSCREEN")),
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Above,
        } => enabled(has_atom("_NET_WM_STATE_ABOVE")),
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Sticky,
        } => enabled(snapshot.state.sticky),
    }
}

fn translate_geometry_result(
    requested: WindowRef,
    command: xenoteer_protocol::WindowMoveResizeCommand,
    _pre_effect: &WindowSnapshot,
    post_effect: Option<WindowSnapshot>,
    evidence: RawWindowControlEvidence,
) -> Result<WindowMoveResizeResult, EffectStage> {
    let raw = match evidence.observed {
        RawWindowControlObservation::Geometry(raw) => raw,
        RawWindowControlObservation::NotObserved => {
            return Err(
                if evidence.outcome == RawWindowControlOutcome::InvalidGeometry {
                    EffectStage::None
                } else {
                    EffectStage::OutcomeUnknown
                },
            );
        }
        _ => return Err(EffectStage::OutcomeUnknown),
    };
    if raw.effective.coordinate_space != CoordinateSpace::RootPhysical
        || raw.client_request.validate().is_err()
        || (raw.bounds_constrained
            && command.bounds_policy != WindowScreenBoundsPolicy::ClampToRoot)
    {
        return Err(EffectStage::OutcomeUnknown);
    }
    let final_snapshot = post_effect.ok_or(EffectStage::OutcomeUnknown)?;
    let observed = final_snapshot
        .geometry
        .clone()
        .ok_or(EffectStage::OutcomeUnknown)?;
    if observed != raw.observed {
        return Err(EffectStage::OutcomeUnknown);
    }
    let observed_rect = match command.relative_to {
        WindowGeometryTarget::Frame => observed.frame_rect,
        WindowGeometryTarget::Client => Some(observed.client_rect),
    };
    let desired_matches =
        observed_rect.is_some_and(|rect| geometry_request_matches(rect, command.geometry));
    let constrained = raw.bounds_constrained || !desired_matches;
    let converged = evidence.outcome == RawWindowControlOutcome::Converged
        && raw.quiet
        && observed_rect == Some(raw.effective);
    let mut warnings = evidence.warnings;
    set_warning(
        &mut warnings,
        WindowControlWarning::GeometryConstrained,
        constrained,
    );
    Ok(WindowMoveResizeResult {
        requested,
        relative_to: command.relative_to,
        desired: command.geometry,
        effective: raw.effective,
        observed,
        observed_revision: final_snapshot.model_revision,
        constrained,
        converged,
        warnings,
    })
}

fn geometry_request_matches(rect: WindowRect, request: WindowGeometryRequest) -> bool {
    let origin = rect.rect.origin();
    let Ok(size) = rect.rect.size() else {
        return false;
    };
    request.x.is_none_or(|value| value == origin.x())
        && request.y.is_none_or(|value| value == origin.y())
        && request.width.is_none_or(|value| value == size.width())
        && request.height.is_none_or(|value| value == size.height())
}

fn set_warning(
    warnings: &mut Vec<WindowControlWarning>,
    warning: WindowControlWarning,
    present: bool,
) {
    warnings.retain(|candidate| *candidate != warning);
    if present {
        warnings.push(warning);
    }
}

fn window_result_stage(
    result: &WindowControlResult,
    raw_outcome: RawWindowControlOutcome,
) -> EffectStage {
    let converged = match result {
        WindowControlResult::Activated(result) => result.converged,
        WindowControlResult::Closed(result) => matches!(
            result.outcome,
            WindowCloseOutcome::Destroyed | WindowCloseOutcome::Unmapped
        ),
        WindowControlResult::StateChanged(result) | WindowControlResult::Minimized(result) => {
            result.converged
        }
        WindowControlResult::GeometryChanged(result) => result.converged,
        WindowControlResult::WorkspaceChanged(result) => result.converged,
        WindowControlResult::Stacked(result) => result.converged,
    };
    if converged || raw_outcome == RawWindowControlOutcome::TargetVanished {
        EffectStage::WindowStateChanged
    } else {
        EffectStage::WindowRequestSent
    }
}

fn map_window_submit_error(error: WindowControlSubmitError) -> RuntimeResult {
    match error {
        WindowControlSubmitError::InvalidRequest(_) => invalid_window_control(),
        WindowControlSubmitError::QueueFull => window_resource_exhausted(),
        WindowControlSubmitError::Closed => capability_unavailable(),
    }
}

fn map_window_actor_failure(kind: WindowControlActorFailureKind) -> RuntimeResult {
    match kind {
        WindowControlActorFailureKind::StaleReference => stale_window_reference(),
        WindowControlActorFailureKind::RevalidationRejected => backend_failure(EffectStage::None),
        WindowControlActorFailureKind::MalformedWindowManagerData => {
            backend_failure(EffectStage::None)
        }
        WindowControlActorFailureKind::CapabilityProbeFailed => capability_unavailable(),
        WindowControlActorFailureKind::ControlQueueFull => window_resource_exhausted(),
        WindowControlActorFailureKind::BackendUnavailable => {
            backend_failure(EffectStage::OutcomeUnknown)
        }
        WindowControlActorFailureKind::ActorPoisoned
        | WindowControlActorFailureKind::ActorStopped
        | WindowControlActorFailureKind::ActorPanicked => capability_unavailable(),
    }
}

fn map_window_model_error(error: ControlPlaneError, stage: EffectStage) -> RuntimeResult {
    match error {
        ControlPlaneError::NotFound
        | ControlPlaneError::StaleReference { .. }
        | ControlPlaneError::PermissionDenied => stale_window_reference(),
        ControlPlaneError::ResourceExhausted => window_resource_exhausted(),
        ControlPlaneError::CapabilityUnavailable => capability_unavailable(),
        _ => backend_failure(stage),
    }
}

fn invalid_window_control() -> RuntimeResult {
    RuntimeResult::failure(
        400,
        ErrorCode::InvalidRequest,
        "Invalid window-control command",
        "The window-control command cannot be represented safely by the active backend.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn unsupported_window_control() -> RuntimeResult {
    RuntimeResult::failure(
        422,
        ErrorCode::UnsupportedByTarget,
        "Window operation unsupported",
        "The active window manager cannot perform this operation with the requested policy.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn stale_window_reference() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::StaleReference,
        "Stale window reference",
        "The exact window birth is no longer live in the current desktop model.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn text_target_not_focused(effect_stage: EffectStage) -> RuntimeResult {
    RuntimeResult::failure(
        422,
        ErrorCode::UnsupportedByTarget,
        "Text target did not acquire focus",
        "The exact target window could not be proven focused before text input.",
        RetryAdvice::Never,
        effect_stage,
    )
}

fn window_resource_exhausted() -> RuntimeResult {
    RuntimeResult::failure(
        429,
        ErrorCode::ResourceExhausted,
        "Window-control capacity exhausted",
        "The bounded window-control queue is currently full.",
        RetryAdvice::AfterBackoff,
        EffectStage::None,
    )
}

fn input_context(command_id: CommandId, context: &ExecutionContext) -> ActionContext {
    ActionContext::new(command_id, context.deadline().map(Instant::into_std))
}

fn input_motion_options(
    curve: PointerCurve,
    duration_ms: Option<u32>,
    policy: MotionPolicy,
) -> Result<MotionOptions, Box<RuntimeResult>> {
    let curve = match curve {
        PointerCurve::Instant => MotionCurve::Instant,
        PointerCurve::Linear => MotionCurve::Linear,
        PointerCurve::Smooth => MotionCurve::Smooth,
    };
    MotionOptions::new(curve, duration_ms, policy, false).map_err(|_| Box::new(invalid_input()))
}

const fn input_logical_button(button: PointerLogicalButton) -> LogicalButton {
    match button {
        PointerLogicalButton::Left => LogicalButton::Left,
        PointerLogicalButton::Middle => LogicalButton::Middle,
        PointerLogicalButton::Right => LogicalButton::Right,
        PointerLogicalButton::Back => LogicalButton::Back,
        PointerLogicalButton::Forward => LogicalButton::Forward,
    }
}

async fn execute_keyboard_command(
    executor: &RuntimeExecutor,
    command_id: CommandId,
    action: Result<KeyboardAction, xenoteer_x11::input::KeyboardActionError>,
    context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    let action = match action {
        Ok(action) => action,
        Err(_) => return completed(invalid_input()),
    };
    let Some(input) = executor.input.borrow().clone() else {
        return completed(capability_unavailable());
    };
    let cancellation = CancellationToken::new();
    let receiver = input.try_submit_keyboard(
        input_context(command_id, &context),
        action,
        cancellation.clone(),
    );
    await_input(receiver, context, cancellation, InputStage::KeyboardAction).await
}

async fn execute_pointer_click_command(
    executor: &RuntimeExecutor,
    command_id: CommandId,
    request: xenoteer_protocol::PointerClickCommand,
    context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    let options =
        match input_motion_options(request.curve, request.duration_ms, executor.motion_policy) {
            Ok(options) => options,
            Err(result) => return completed(*result),
        };
    let Some(input) = executor.input.borrow().clone() else {
        return completed(capability_unavailable());
    };
    let cancellation = CancellationToken::new();
    let button = input_logical_button(request.button);
    let basic = |endpoint| {
        InputOperation::PointerClick(PointerClickRequest::new(
            endpoint,
            options,
            button,
            request.count,
            request.pre_click_dwell_ms,
            request.press_duration_ms,
            request.inter_click_interval_ms,
        ))
    };
    let (receiver, prior_stage) = match request.target {
        PointerClickTarget::Current => (
            input.try_submit_operation(
                input_context(command_id, &context),
                basic(None),
                cancellation.clone(),
            ),
            EffectStage::None,
        ),
        PointerClickTarget::Root { point } => {
            let target = match RootPoint::try_from_protocol(point) {
                Ok(target) => target,
                Err(_) => return completed(invalid_input()),
            };
            (
                input.try_submit_operation(
                    input_context(command_id, &context),
                    basic(Some(PointerEndpoint::Root(target))),
                    cancellation.clone(),
                ),
                EffectStage::None,
            )
        }
        PointerClickTarget::Window {
            window,
            coordinate_space,
            point,
            activate,
            bounds_policy,
        } => {
            let Some(window_control) = executor.window_control.clone() else {
                return completed(capability_unavailable());
            };
            let prior_stage = if activate {
                let activation_target = window.clone();
                let activation_runtime = window_control.clone();
                let activation = tokio::task::spawn_blocking(move || {
                    activation_runtime.execute(Command::WindowActivate(WindowActivateCommand {
                        window: activation_target,
                        switch_workspace: true,
                        fallback: WindowFocusFallback::EwmhOnly,
                    }))
                })
                .await
                .unwrap_or_else(|_| backend_failure(EffectStage::OutcomeUnknown));
                if matches!(activation, RuntimeResult::Failure(_)) {
                    return completed(activation);
                }
                activation.effect_stage()
            } else {
                EffectStage::None
            };
            let coordinate_space = match coordinate_space {
                WindowPointerCoordinateSpace::Client => CoordinateSpace::WindowClient,
                WindowPointerCoordinateSpace::Frame => CoordinateSpace::WindowFrame,
            };
            let bounds_policy = match bounds_policy {
                WireWindowPointerBoundsPolicy::Reject => X11WindowPointerBoundsPolicy::Reject,
                WireWindowPointerBoundsPolicy::Clamp => X11WindowPointerBoundsPolicy::Clamp,
                WireWindowPointerBoundsPolicy::Allow => X11WindowPointerBoundsPolicy::Allow,
            };
            let operation = InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
                window.xid,
                coordinate_space,
                point,
                bounds_policy,
                options,
                button,
                request.count,
                request.pre_click_dwell_ms,
                request.press_duration_ms,
                request.inter_click_interval_ms,
            ));
            let precondition = window_click_precondition_spec(window);
            let precondition = exact_window_input_precondition(
                Arc::clone(&window_control.observation),
                precondition.target,
                precondition.require_focus,
            );
            (
                input.try_submit_operation_with_precondition(
                    input_context(command_id, &context),
                    operation,
                    precondition,
                    cancellation.clone(),
                ),
                prior_stage,
            )
        }
    };
    await_input_after_stage(
        receiver,
        context,
        cancellation,
        InputStage::PointerClick,
        prior_stage,
    )
    .await
}

fn exact_window_input_precondition(
    observation: Arc<DaemonObservationService>,
    target: WindowRef,
    require_focus: bool,
) -> InputPrecondition {
    InputPrecondition::new(move || {
        let snapshot = observation
            .snapshot_exact_blocking(target.clone(), WINDOW_REVALIDATION_TIMEOUT)
            .map_err(|error| match error {
                ControlPlaneError::NotFound
                | ControlPlaneError::StaleReference { .. }
                | ControlPlaneError::PermissionDenied => InputPreconditionFailure::TargetStale,
                _ => InputPreconditionFailure::Unavailable,
            })?;
        if require_focus && !snapshot.state.focused {
            return Err(InputPreconditionFailure::FocusLost);
        }
        Ok(())
    })
}

fn window_click_precondition_spec(target: WindowRef) -> WindowInputPreconditionSpec {
    WindowInputPreconditionSpec {
        target,
        require_focus: true,
    }
}

#[derive(Clone, Copy)]
enum InputStage {
    PointerMove,
    PointerClick,
    PointerDrag,
    PointerScroll,
    ButtonDown,
    ButtonUp,
    KeyDown,
    KeyUp,
    KeyboardAction,
}

impl InputStage {
    const fn after_effect(self) -> EffectStage {
        match self {
            Self::PointerMove => EffectStage::PointerMoved,
            Self::PointerClick => EffectStage::PointerClicked,
            Self::PointerDrag => EffectStage::PointerDragged,
            Self::PointerScroll => EffectStage::PointerScrolled,
            Self::ButtonDown => EffectStage::ButtonPressed,
            Self::ButtonUp => EffectStage::ButtonReleased,
            Self::KeyDown => EffectStage::KeyPressed,
            Self::KeyUp => EffectStage::KeyReleased,
            Self::KeyboardAction => EffectStage::KeyboardActionCompleted,
        }
    }
}

async fn await_input(
    receiver: Result<oneshot::Receiver<Result<InputOutcome, InputFailure>>, InputSubmitError>,
    context: ExecutionContext,
    cancellation: CancellationToken,
    stage: InputStage,
) -> ExecutionOutcome<RuntimeResult> {
    await_input_after_stage(receiver, context, cancellation, stage, EffectStage::None).await
}

async fn await_input_after_stage(
    receiver: Result<oneshot::Receiver<Result<InputOutcome, InputFailure>>, InputSubmitError>,
    mut context: ExecutionContext,
    cancellation: CancellationToken,
    stage: InputStage,
    prior_stage: EffectStage,
) -> ExecutionOutcome<RuntimeResult> {
    let mut receiver = match receiver {
        Ok(receiver) => receiver,
        Err(InputSubmitError::QueueFull) => {
            return completed(resource_exhausted().preserve_prior_effect(prior_stage));
        }
        Err(InputSubmitError::Closed) => {
            return completed(capability_unavailable().preserve_prior_effect(prior_stage));
        }
    };
    tokio::select! {
        result = &mut receiver => map_input_result(result, stage, prior_stage),
        _reason = context.wait_for_stop() => {
            cancellation.cancel();
            let input_effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut receiver).await {
                Ok(Ok(Ok(outcome))) if outcome.events_emitted > 0 => CommandEffect::AfterEffect,
                Ok(Ok(Err(failure))) if failure.events_emitted > 0 || !failure.progress_known => {
                    CommandEffect::AfterEffect
                }
                Err(_) => CommandEffect::AfterEffect,
                _ => CommandEffect::BeforeEffect,
            };
            ExecutionOutcome::Stopped {
                effect: if prior_stage.has_visible_effect() {
                    CommandEffect::AfterEffect
                } else {
                    input_effect
                },
            }
        }
    }
}

fn map_input_result(
    result: Result<Result<InputOutcome, InputFailure>, oneshot::error::RecvError>,
    stage: InputStage,
    prior_stage: EffectStage,
) -> ExecutionOutcome<RuntimeResult> {
    match result {
        Ok(Ok(outcome)) => {
            let effect_stage = precise_input_effect_stage(
                outcome.events_emitted,
                outcome.completed_units,
                true,
                Some(&outcome.effects),
                stage,
                prior_stage,
            );
            match outcome.kind {
                InputOutcomeKind::Completed => completed(RuntimeResult::success(
                    CommandOutcome::Acknowledged,
                    effect_stage,
                )),
                InputOutcomeKind::CancelledAfterEffect => completed(RuntimeResult::failure(
                    409,
                    ErrorCode::CancelledAfterEffect,
                    "Command cancelled after effect",
                    "Cancellation was observed after a physical input effect.",
                    RetryAdvice::Never,
                    effect_stage,
                )),
                InputOutcomeKind::DeadlineExceededAfterEffect => completed(RuntimeResult::failure(
                    504,
                    ErrorCode::DeadlineExceededAfterEffect,
                    "Command deadline exceeded after effect",
                    "The deadline elapsed after a physical input effect.",
                    RetryAdvice::Never,
                    effect_stage,
                )),
            }
        }
        Ok(Err(failure)) => completed(input_failure(failure, stage, prior_stage)),
        Err(_) => completed(
            backend_failure(EffectStage::SideEffectObserved).preserve_prior_effect(prior_stage),
        ),
    }
}

fn input_failure(
    failure: InputFailure,
    stage: InputStage,
    prior_stage: EffectStage,
) -> RuntimeResult {
    let effect_stage = precise_input_effect_stage(
        failure.events_emitted,
        failure.completed_units,
        failure.progress_known,
        failure.effects.as_deref(),
        stage,
        prior_stage,
    );
    match failure.kind {
        InputFailureKind::CancelledBeforeEffect if prior_stage.has_visible_effect() => {
            RuntimeResult::failure(
                409,
                ErrorCode::CancelledAfterEffect,
                "Command cancelled after effect",
                "Cancellation was observed after the window activation effect.",
                RetryAdvice::Never,
                prior_stage,
            )
        }
        InputFailureKind::CancelledBeforeEffect => RuntimeResult::failure(
            409,
            ErrorCode::CancelledBeforeEffect,
            "Command cancelled before effect",
            "Cancellation was observed before physical input changed the desktop.",
            RetryAdvice::SameCommandId,
            EffectStage::None,
        ),
        InputFailureKind::DeadlineExceededBeforeEffect if prior_stage.has_visible_effect() => {
            RuntimeResult::failure(
                504,
                ErrorCode::DeadlineExceededAfterEffect,
                "Command deadline exceeded after effect",
                "The deadline elapsed after the window activation effect.",
                RetryAdvice::Never,
                prior_stage,
            )
        }
        InputFailureKind::DeadlineExceededBeforeEffect => RuntimeResult::failure(
            504,
            ErrorCode::DeadlineExceededBeforeEffect,
            "Command deadline exceeded before effect",
            "The deadline elapsed before physical input changed the desktop.",
            RetryAdvice::SameCommandId,
            EffectStage::None,
        ),
        InputFailureKind::UnsupportedOperation | InputFailureKind::UnsupportedByBackend => {
            RuntimeResult::failure(
                422,
                ErrorCode::UnsupportedByTarget,
                "Input operation unsupported",
                "The active X11 input backend cannot perform this operation safely.",
                RetryAdvice::Never,
                effect_stage,
            )
        }
        InputFailureKind::HealthRejected
        | InputFailureKind::ActorStopped
        | InputFailureKind::ActorPanicked => RuntimeResult::failure(
            503,
            ErrorCode::CapabilityUnavailable,
            "Input capability unavailable",
            "The physical input actor is not healthy enough to accept this operation.",
            RetryAdvice::AfterBackoff,
            effect_stage,
        ),
        InputFailureKind::ControlQueueFull => resource_exhausted(),
        InputFailureKind::TargetStale => {
            stale_window_reference().preserve_prior_effect(effect_stage)
        }
        InputFailureKind::FocusLost => text_target_not_focused(effect_stage),
        InputFailureKind::PreconditionUnavailable => backend_failure(effect_stage),
        _ => backend_failure(effect_stage),
    }
}

fn precise_input_effect_stage(
    events_emitted: usize,
    completed_units: u16,
    progress_known: bool,
    effects: Option<&InputEffectEvidence>,
    requested_stage: InputStage,
    prior_stage: EffectStage,
) -> EffectStage {
    if !progress_known {
        return requested_stage.after_effect();
    }
    if events_emitted == 0 {
        return prior_stage;
    }
    if completed_units > 0 {
        return requested_stage.after_effect();
    }
    let Some(InputEffectEvidence::Journal(journal)) = effects else {
        return requested_stage.after_effect();
    };
    journal
        .records()
        .last()
        .map(|record| input_effect_stage(record.effect()))
        .unwrap_or(prior_stage)
}

const fn input_effect_stage(effect: Effect) -> EffectStage {
    match effect {
        Effect::PointerMoved { .. } => EffectStage::PointerMoved,
        Effect::ButtonPressed { .. } => EffectStage::ButtonPressed,
        Effect::ButtonReleased { .. } => EffectStage::ButtonReleased,
        Effect::KeyPressed { .. } => EffectStage::KeyPressed,
        Effect::KeyReleased { .. } => EffectStage::KeyReleased,
    }
}

#[derive(Clone, Copy)]
enum ProcessOperation {
    Launch,
    Status,
    Terminate,
}

async fn await_process<F, T>(
    future: F,
    mut context: ExecutionContext,
    operation: ProcessOperation,
) -> ExecutionOutcome<RuntimeResult>
where
    F: Future<Output = Result<T, BrokerClientError>> + Send,
    T: IntoProcessResult,
{
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => process_completion(result, operation),
        _reason = context.wait_for_stop() => {
            match future.await {
                Ok(result) if operation.mutates_processes() => {
                    // Launch and termination are bounded atomic primitives.
                    // Preserve their identity-bearing result even when the
                    // caller deadline or stop races the broker reply.
                    process_completion(Ok(result), operation)
                }
                Err(error) if process_error_may_have_effect(&error, operation) => {
                    ExecutionOutcome::Stopped {
                        effect: CommandEffect::AfterEffect,
                    }
                }
                Ok(_) | Err(_) => ExecutionOutcome::Stopped {
                    effect: CommandEffect::BeforeEffect,
                },
            }
        }
    }
}

fn process_completion<T: IntoProcessResult>(
    result: Result<T, BrokerClientError>,
    operation: ProcessOperation,
) -> ExecutionOutcome<RuntimeResult> {
    let atomic = result.is_ok() && operation.mutates_processes();
    let output = map_process_result(result, operation);
    if atomic {
        let effect = if output.effect_stage().has_visible_effect() {
            CommandEffect::AfterEffect
        } else {
            CommandEffect::BeforeEffect
        };
        ExecutionOutcome::AtomicCompleted { output, effect }
    } else {
        completed(output)
    }
}

trait IntoProcessResult {
    fn into_outcome(self, operation: ProcessOperation) -> CommandOutcome;
}

impl IntoProcessResult for xenoteer_protocol::ProcessRef {
    fn into_outcome(self, _operation: ProcessOperation) -> CommandOutcome {
        CommandOutcome::ApplicationLaunched { process: self }
    }
}

impl IntoProcessResult for xenoteer_protocol::ProcessView {
    fn into_outcome(self, operation: ProcessOperation) -> CommandOutcome {
        match operation {
            ProcessOperation::Status => CommandOutcome::ProcessStatus { process: self },
            ProcessOperation::Terminate => CommandOutcome::ProcessTerminated { process: self },
            ProcessOperation::Launch => CommandOutcome::ProcessStatus { process: self },
        }
    }
}

fn map_process_result<T: IntoProcessResult>(
    result: Result<T, BrokerClientError>,
    operation: ProcessOperation,
) -> RuntimeResult {
    match result {
        Ok(result) => {
            let effect_stage = match operation {
                ProcessOperation::Launch => EffectStage::ProcessStarted,
                ProcessOperation::Status => EffectStage::None,
                ProcessOperation::Terminate => EffectStage::ProcessExited,
            };
            RuntimeResult::success(result.into_outcome(operation), effect_stage)
        }
        Err(BrokerClientError::Rejected { code }) => process_rejection(code, operation),
        Err(error) => backend_failure(if process_error_may_have_effect(&error, operation) {
            EffectStage::SideEffectObserved
        } else {
            EffectStage::None
        }),
    }
}

fn process_rejection(code: BrokerErrorCode, operation: ProcessOperation) -> RuntimeResult {
    let (status, error_code, title, detail, retry, effect_stage) = match code {
        BrokerErrorCode::InvalidRequest => (
            422,
            ErrorCode::InvalidRequest,
            "Invalid process request",
            "The process operation violates the registered application policy.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        BrokerErrorCode::OperationIdConflict => (
            409,
            ErrorCode::CommandIdConflict,
            "Process operation ID conflict",
            "The process operation ID is retained with different launch content.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        BrokerErrorCode::ApplicationNotRegistered => (
            422,
            ErrorCode::UnsupportedByTarget,
            "Application not registered",
            "The requested application is not present in the image-owned registry.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        BrokerErrorCode::ProcessLimitExceeded => (
            429,
            ErrorCode::ResourceExhausted,
            "Process limit reached",
            "The bounded managed-process capacity is currently exhausted.",
            RetryAdvice::AfterBackoff,
            EffectStage::None,
        ),
        BrokerErrorCode::WrongDesktopGeneration | BrokerErrorCode::ProcessReferenceMismatch => (
            409,
            ErrorCode::StaleReference,
            "Stale process reference",
            "The process reference does not identify the current managed desktop process.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        BrokerErrorCode::ProcessNotManaged => (
            404,
            ErrorCode::NotFound,
            "Process not found",
            "The exact process reference is not retained by the managed-process broker.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        BrokerErrorCode::TerminationInProgress => (
            409,
            ErrorCode::ResourceExhausted,
            "Termination already in progress",
            "Another bounded termination operation already owns this process.",
            RetryAdvice::AfterBackoff,
            EffectStage::None,
        ),
        BrokerErrorCode::ManagerUnavailable => (
            503,
            ErrorCode::CapabilityUnavailable,
            "Process manager unavailable",
            "The managed-process capability is temporarily unavailable.",
            RetryAdvice::AfterBackoff,
            EffectStage::None,
        ),
        BrokerErrorCode::SpawnFailed => (
            502,
            ErrorCode::BackendFailure,
            "Application launch failed",
            "The registered application could not be started.",
            RetryAdvice::AfterBackoff,
            EffectStage::None,
        ),
        BrokerErrorCode::Internal => (
            500,
            ErrorCode::Internal,
            "Process manager failure",
            "The managed-process broker encountered an internal failure.",
            RetryAdvice::Never,
            if operation.mutates_processes() {
                EffectStage::SideEffectObserved
            } else {
                EffectStage::None
            },
        ),
    };
    RuntimeResult::failure(status, error_code, title, detail, retry, effect_stage)
}

impl ProcessOperation {
    const fn mutates_processes(self) -> bool {
        matches!(self, Self::Launch | Self::Terminate)
    }
}

fn process_error_may_have_effect(error: &BrokerClientError, operation: ProcessOperation) -> bool {
    if !operation.mutates_processes() {
        return false;
    }
    match error {
        BrokerClientError::Connect(_) => false,
        BrokerClientError::Rejected { code } => matches!(code, BrokerErrorCode::Internal),
        BrokerClientError::Timeout
        | BrokerClientError::Transport(_)
        | BrokerClientError::UnexpectedReply => true,
    }
}

fn completed(result: RuntimeResult) -> ExecutionOutcome<RuntimeResult> {
    let effect = if result.effect_stage().has_visible_effect() {
        CommandEffect::AfterEffect
    } else {
        CommandEffect::BeforeEffect
    };
    ExecutionOutcome::Completed {
        output: result,
        effect,
    }
}

fn invalid_input() -> RuntimeResult {
    RuntimeResult::failure(
        400,
        ErrorCode::InvalidRequest,
        "Invalid input command",
        "The command cannot be represented safely by the active input backend.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn invalid_clipboard_request() -> RuntimeResult {
    RuntimeResult::failure(
        400,
        ErrorCode::InvalidRequest,
        "Invalid clipboard command",
        "The clipboard or text command failed execution-time validation.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn stale_clipboard_artifact() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::StaleReference,
        "Stale clipboard artifact",
        "The immutable clipboard input reference no longer matches its stored object.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn clipboard_has_no_owner() -> RuntimeResult {
    RuntimeResult::failure(
        404,
        ErrorCode::NotFound,
        "Clipboard selection has no owner",
        "The requested X11 selection does not currently have an owner.",
        RetryAdvice::AfterBackoff,
        EffectStage::None,
    )
}

fn clipboard_preservation_failed() -> RuntimeResult {
    RuntimeResult::failure(
        422,
        ErrorCode::UnsupportedByTarget,
        "Clipboard cannot be preserved safely",
        "The previous clipboard value is not bounded UTF-8 text that can be copied honestly.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn paste_not_observed() -> RuntimeResult {
    RuntimeResult::failure(
        502,
        ErrorCode::BackendFailure,
        "Paste transfer was not observed",
        "The target did not complete a compatible clipboard transfer after the paste chord.",
        RetryAdvice::Never,
        EffectStage::TextInserted,
    )
}

fn unsupported_text_strategy() -> RuntimeResult {
    unsupported_text_strategy_at(EffectStage::None)
}

fn unsupported_text_strategy_at(effect_stage: EffectStage) -> RuntimeResult {
    RuntimeResult::failure(
        422,
        ErrorCode::UnsupportedByTarget,
        "Text strategy unsupported",
        "The selected text strategy cannot represent the request safely on this desktop.",
        RetryAdvice::Never,
        effect_stage,
    )
}

fn clipboard_resource_exhausted() -> RuntimeResult {
    RuntimeResult::failure(
        429,
        ErrorCode::ResourceExhausted,
        "Clipboard capacity exhausted",
        "The bounded clipboard request capacity is currently exhausted.",
        RetryAdvice::AfterBackoff,
        EffectStage::None,
    )
}

fn clipboard_outcome_unknown() -> RuntimeResult {
    RuntimeResult::failure(
        504,
        ErrorCode::RequestOutcomeUnknown,
        "Clipboard request outcome unknown",
        "The clipboard actor did not return enough evidence to prove the ownership outcome.",
        RetryAdvice::SameCommandId,
        EffectStage::OutcomeUnknown,
    )
}

fn resource_exhausted() -> RuntimeResult {
    RuntimeResult::failure(
        429,
        ErrorCode::ResourceExhausted,
        "Input capacity exhausted",
        "The bounded input queue is currently full.",
        RetryAdvice::AfterBackoff,
        EffectStage::None,
    )
}

fn capability_unavailable() -> RuntimeResult {
    capability_unavailable_at(EffectStage::None)
}

fn capability_unavailable_at(effect_stage: EffectStage) -> RuntimeResult {
    RuntimeResult::failure(
        503,
        ErrorCode::CapabilityUnavailable,
        "Desktop capability unavailable",
        "The required desktop subsystem is not ready.",
        RetryAdvice::AfterBackoff,
        effect_stage,
    )
}

fn permission_denied() -> RuntimeResult {
    RuntimeResult::failure(
        403,
        ErrorCode::PermissionDenied,
        "Command permission denied",
        "The authenticated principal is not authorized at the execution boundary.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn stale_generation(generation: DesktopGeneration) -> RuntimeResult {
    let _generation = generation;
    RuntimeResult::failure(
        409,
        ErrorCode::StaleReference,
        "Desktop generation changed",
        "The command no longer belongs to the active desktop lifetime.",
        RetryAdvice::AfterResync,
        EffectStage::None,
    )
}

fn backend_failure(effect_stage: EffectStage) -> RuntimeResult {
    RuntimeResult::failure(
        502,
        ErrorCode::BackendFailure,
        "Backend operation failed",
        "An external desktop backend could not prove successful completion.",
        RetryAdvice::Never,
        effect_stage,
    )
}

struct CoordinatorControlPlane {
    handle: RuntimeHandle,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    default_timeout: Duration,
    clock: ClockProjection,
    #[cfg(test)]
    wait_started: Option<Arc<tokio::sync::Semaphore>>,
}

struct RuntimeLiveEventReceiver {
    live: broadcast::Receiver<u64>,
    handle: RuntimeHandle,
    principal: PrincipalId,
    desktop_id: DesktopId,
    generation: GenerationToken,
    last_global_sequence: u64,
}

impl LiveEventReceiver for RuntimeLiveEventReceiver {
    fn receive<'a>(&'a mut self) -> ControlFuture<'a, LiveEvent> {
        Box::pin(async move {
            loop {
                match self.live.recv().await {
                    Ok(sequence) => {
                        let since = sequence.saturating_sub(1);
                        let replay = self.handle.replay_events(self.generation, since).await;
                        let (record, latest_sequence) = match replay {
                            Ok(ReplayResult::Events {
                                latest_sequence,
                                events,
                                ..
                            }) => (
                                events
                                    .into_iter()
                                    .find(|record| record.sequence == sequence),
                                latest_sequence,
                            ),
                            Ok(ReplayResult::ResyncRequired {
                                reason,
                                current_generation,
                                dropped_through,
                                latest_sequence,
                            }) => {
                                return LiveEvent::ResyncRequired {
                                    reason: project_replay_reason(reason),
                                    desktop_generation: current_generation.generation(),
                                    dropped_through,
                                    latest_sequence,
                                };
                            }
                            Err(_) => return LiveEvent::Closed,
                        };
                        let Some(record) = record else {
                            return LiveEvent::ResyncRequired {
                                reason: EventResyncReason::HistoryLost,
                                desktop_generation: self.generation.generation(),
                                dropped_through: sequence,
                                latest_sequence: sequence,
                            };
                        };
                        self.last_global_sequence = sequence;
                        match project_event(self.desktop_id, &self.principal, record) {
                            Ok(ProjectedEvent::Visible(event)) => {
                                return LiveEvent::Event(event);
                            }
                            Ok(ProjectedEvent::Hidden) => continue,
                            Ok(ProjectedEvent::ResyncBarrier {
                                sequence: barrier_sequence,
                            }) => {
                                return LiveEvent::ResyncRequired {
                                    reason: EventResyncReason::HistoryLost,
                                    desktop_generation: self.generation.generation(),
                                    dropped_through: barrier_sequence,
                                    latest_sequence,
                                };
                            }
                            Err(_) => {
                                return LiveEvent::ResyncRequired {
                                    reason: EventResyncReason::HistoryLost,
                                    desktop_generation: self.generation.generation(),
                                    dropped_through: sequence,
                                    latest_sequence: sequence,
                                };
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let missed_through = self.last_global_sequence.saturating_add(skipped);
                        let replay = self.handle.replay_events(self.generation, u64::MAX).await;
                        return match replay {
                            Ok(ReplayResult::ResyncRequired {
                                current_generation,
                                dropped_through: retained_dropped_through,
                                latest_sequence,
                                ..
                            }) => LiveEvent::ResyncRequired {
                                reason: EventResyncReason::SubscriberLag,
                                desktop_generation: current_generation.generation(),
                                dropped_through: missed_through.max(retained_dropped_through),
                                latest_sequence,
                            },
                            Ok(ReplayResult::Events {
                                latest_sequence, ..
                            }) => LiveEvent::ResyncRequired {
                                reason: EventResyncReason::SubscriberLag,
                                desktop_generation: self.generation.generation(),
                                dropped_through: missed_through,
                                latest_sequence,
                            },
                            Err(_) => LiveEvent::Closed,
                        };
                    }
                    Err(broadcast::error::RecvError::Closed) => return LiveEvent::Closed,
                }
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ProjectedEvent {
    Visible(SequencedEvent),
    Hidden,
    ResyncBarrier { sequence: u64 },
}

fn project_event(
    desktop_id: DesktopId,
    principal: &PrincipalId,
    record: EventRecord<RuntimeEvent>,
) -> Result<ProjectedEvent, ControlPlaneError> {
    let event = match record.event {
        RuntimeEvent::Targeted { audience, event } => {
            if audience != *principal {
                return Ok(ProjectedEvent::Hidden);
            }
            event
        }
        RuntimeEvent::Broadcast { event } => event,
        RuntimeEvent::ResyncBarrier => {
            return Ok(ProjectedEvent::ResyncBarrier {
                sequence: record.sequence,
            });
        }
    };
    let event = SequencedEvent {
        desktop_id,
        desktop_generation: record.generation.generation(),
        sequence: record.sequence,
        topic: event.topic,
        payload: event.payload,
    };
    event.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(ProjectedEvent::Visible(event))
}

fn project_replayed_events(
    desktop_id: DesktopId,
    principal: &PrincipalId,
    generation: GenerationToken,
    latest_sequence: u64,
    events: Vec<EventRecord<RuntimeEvent>>,
) -> Result<EventReplay, ControlPlaneError> {
    let mut projected = Vec::new();
    for record in events {
        match project_event(desktop_id, principal, record)? {
            ProjectedEvent::Visible(event) => projected.push(event),
            ProjectedEvent::Hidden => {}
            ProjectedEvent::ResyncBarrier { sequence } => {
                return Ok(EventReplay::ResyncRequired {
                    reason: EventResyncReason::HistoryLost,
                    desktop_generation: generation.generation(),
                    dropped_through: sequence,
                    latest_sequence,
                });
            }
        }
    }
    Ok(EventReplay::Events {
        latest_sequence,
        events: projected,
    })
}

const fn project_replay_reason(reason: ReplayFailure) -> EventResyncReason {
    match reason {
        ReplayFailure::GenerationChanged => EventResyncReason::GenerationChanged,
        ReplayFailure::HistoryLost => EventResyncReason::HistoryLost,
        ReplayFailure::SequenceAhead => EventResyncReason::SequenceAhead,
    }
}

impl CoordinatorControlPlane {
    fn require_grant(
        context: &ControlRequestContext,
        grant: Grant,
    ) -> Result<(), ControlPlaneError> {
        context
            .principal()
            .has_grant(grant)
            .then_some(())
            .ok_or(ControlPlaneError::PermissionDenied)
    }

    fn require_cancel_grant(context: &ControlRequestContext) -> Result<(), ControlPlaneError> {
        context
            .principal()
            .has_command_cancellation_grant()
            .then_some(())
            .ok_or(ControlPlaneError::PermissionDenied)
    }

    fn principal(context: &ControlRequestContext) -> Result<PrincipalId, ControlPlaneError> {
        PrincipalId::new(context.principal().id()).map_err(|_| ControlPlaneError::Internal)
    }

    async fn token(&self) -> Result<GenerationToken, ControlPlaneError> {
        let token = self
            .handle
            .generation()
            .await
            .map_err(map_coordinator_error)?;
        if token.desktop_id() != self.desktop_id || token.generation() != self.generation {
            return Err(ControlPlaneError::StaleReference {
                current_generation: Some(token.generation()),
            });
        }
        Ok(token)
    }

    fn lease_view(
        &self,
        snapshot: LeaseSnapshot,
        principal: &PrincipalId,
    ) -> Result<LeaseStateView, ControlPlaneError> {
        let view = match snapshot {
            LeaseSnapshot::Vacant { generation } => LeaseStateView {
                desktop_id: self.desktop_id,
                desktop_generation: generation.generation(),
                state: LeaseAvailability::Vacant,
                lease_id: None,
                expires_at: None,
            },
            LeaseSnapshot::Held(lease) if lease.owner == *principal => LeaseStateView {
                desktop_id: self.desktop_id,
                desktop_generation: lease.generation.generation(),
                state: LeaseAvailability::HeldByCaller,
                lease_id: Some(lease.lease_id),
                expires_at: Some(self.clock.timestamp(lease.expires_at)?),
            },
            LeaseSnapshot::Held(lease) => LeaseStateView {
                desktop_id: self.desktop_id,
                desktop_generation: lease.generation.generation(),
                state: LeaseAvailability::Occupied,
                lease_id: None,
                expires_at: Some(self.clock.timestamp(lease.expires_at)?),
            },
            LeaseSnapshot::Revoking { lease, .. } => LeaseStateView {
                desktop_id: self.desktop_id,
                desktop_generation: lease.generation.generation(),
                state: LeaseAvailability::Revoking,
                lease_id: None,
                expires_at: Some(self.clock.timestamp(lease.expires_at)?),
            },
            LeaseSnapshot::Resetting { lease, .. } => LeaseStateView {
                desktop_id: self.desktop_id,
                desktop_generation: lease.generation.generation(),
                state: LeaseAvailability::Resetting,
                lease_id: None,
                expires_at: Some(self.clock.timestamp(lease.expires_at)?),
            },
        };
        view.validate().map_err(|_| ControlPlaneError::Internal)?;
        Ok(view)
    }

    fn map_record(
        &self,
        record: &CommandRecord<CommandTerminal<RuntimeResult>>,
    ) -> Result<CommandResult, ControlPlaneError> {
        let accepted_at = self.clock.timestamp(record.accepted_at)?;
        let updated_at = self.clock.timestamp(record.updated_at)?;
        let accepted = CommandResult::accepted(record.command_id, accepted_at.clone());
        let result = match &record.state {
            CommandRecordState::Accepted => accepted,
            CommandRecordState::Running => accepted
                .start(updated_at)
                .map_err(|_| ControlPlaneError::Internal)?,
            CommandRecordState::Terminal(terminal) => self.map_terminal(
                accepted,
                accepted_at,
                updated_at,
                terminal,
                record.generation.generation(),
            )?,
        };
        result.validate().map_err(|_| ControlPlaneError::Internal)?;
        Ok(result)
    }

    fn map_terminal(
        &self,
        accepted: CommandResult,
        accepted_at: Timestamp,
        finished_at: Timestamp,
        terminal: &CommandTerminal<RuntimeResult>,
        generation: DesktopGeneration,
    ) -> Result<CommandResult, ControlPlaneError> {
        if terminal.cause == TerminalCause::Returned {
            let output = terminal
                .output
                .as_ref()
                .ok_or(ControlPlaneError::Internal)?;
            return match output {
                RuntimeResult::Success(success) => accepted
                    .start(accepted_at)
                    .and_then(|running| {
                        running.succeed(success.effect_stage, success.outcome.clone(), finished_at)
                    })
                    .map_err(|_| ControlPlaneError::Internal),
                RuntimeResult::Failure(failure) => {
                    let problem = runtime_problem(failure, generation)?;
                    let lifecycle = lifecycle_for_failure(failure);
                    let result = if failure.effect_stage.has_visible_effect()
                        || lifecycle == CommandLifecycle::Failed
                    {
                        accepted
                            .start(accepted_at)
                            .and_then(|running| running.fail(lifecycle, problem, finished_at))
                    } else {
                        accepted.fail(lifecycle, problem, finished_at)
                    };
                    result.map_err(|_| ControlPlaneError::Internal)
                }
            };
        }

        let (lifecycle, failure) = terminal_failure(terminal, generation);
        let problem = runtime_problem(&failure, generation)?;
        let result =
            if failure.effect_stage.has_visible_effect() || lifecycle == CommandLifecycle::Failed {
                accepted
                    .start(accepted_at)
                    .and_then(|running| running.fail(lifecycle, problem, finished_at))
            } else {
                accepted.fail(lifecycle, problem, finished_at)
            };
        result.map_err(|_| ControlPlaneError::Internal)
    }
}

impl ControlPlane for CoordinatorControlPlane {
    fn lease_state<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::InputControl)?;
            if desktop_id != self.desktop_id {
                return Err(ControlPlaneError::NotFound);
            }
            let principal = Self::principal(&context)?;
            let _token = self.token().await?;
            let snapshot = self
                .handle
                .lease_snapshot()
                .await
                .map_err(map_coordinator_error)?;
            self.lease_view(snapshot, &principal)
        })
    }

    fn acquire_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseAcquireRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::InputControl)?;
            request
                .validate()
                .map_err(|_| ControlPlaneError::InvalidRequest)?;
            let principal = Self::principal(&context)?;
            let token = self.token().await?;
            let lease_id = ControlLeaseId::new();
            self.handle
                .acquire_lease(
                    principal.clone(),
                    lease_id,
                    request.ttl_ms.map(u64::from),
                    token,
                )
                .await
                .map_err(map_coordinator_error)?;
            let snapshot = self
                .handle
                .lease_snapshot()
                .await
                .map_err(map_coordinator_error)?;
            self.lease_view(snapshot, &principal)
        })
    }

    fn renew_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseRenewRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::InputControl)?;
            request
                .validate()
                .map_err(|_| ControlPlaneError::InvalidRequest)?;
            let principal = Self::principal(&context)?;
            let token = self.token().await?;
            self.handle
                .renew_lease(
                    principal.clone(),
                    request.lease_id,
                    request.ttl_ms.map(u64::from),
                    token,
                )
                .await
                .map_err(map_coordinator_error)?;
            let snapshot = self
                .handle
                .lease_snapshot()
                .await
                .map_err(map_coordinator_error)?;
            self.lease_view(snapshot, &principal)
        })
    }

    fn release_lease<'a>(
        &'a self,
        context: ControlRequestContext,
        request: LeaseReleaseRequest,
    ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::InputControl)?;
            request
                .validate()
                .map_err(|_| ControlPlaneError::InvalidRequest)?;
            let principal = Self::principal(&context)?;
            let token = self.token().await?;
            self.handle
                .release_lease(principal.clone(), request.lease_id, token)
                .await
                .map_err(map_coordinator_error)?;
            let deadline = Instant::now() + INPUT_CONTROL_TIMEOUT;
            loop {
                let snapshot = self
                    .handle
                    .lease_snapshot()
                    .await
                    .map_err(map_coordinator_error)?;
                if matches!(snapshot, LeaseSnapshot::Vacant { .. }) {
                    return self.lease_view(snapshot, &principal);
                }
                if Instant::now() >= deadline {
                    return Err(ControlPlaneError::CapabilityUnavailable);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }

    fn submit_command<'a>(
        &'a self,
        context: ControlRequestContext,
        envelope: CommandEnvelope,
    ) -> ControlFuture<'a, Result<CommandSubmission, ControlPlaneError>> {
        Box::pin(async move {
            context
                .principal()
                .satisfies(command_grant_requirement(&envelope.command))
                .then_some(())
                .ok_or(ControlPlaneError::PermissionDenied)?;
            envelope
                .validate()
                .map_err(|_| ControlPlaneError::InvalidRequest)?;
            if envelope.desktop_id != self.desktop_id
                || envelope.desktop_generation != self.generation
            {
                return Err(ControlPlaneError::StaleReference {
                    current_generation: Some(self.generation),
                });
            }
            let principal = Self::principal(&context)?;
            let generation = self.token().await?;
            let hash = canonical_hash(&envelope)?;
            let lease = match envelope.lease_id {
                Some(lease_id) if envelope.command.requires_control_lease() => {
                    LeaseRequirement::Required(lease_id)
                }
                _ => LeaseRequirement::NotRequired,
            };
            let execute_within =
                deadline_duration(envelope.deadline.as_ref(), self.default_timeout)?;
            let command = RuntimeCommand {
                command_id: envelope.command_id,
                principal: principal.clone(),
                authorization: context.principal().clone(),
                command: envelope.command,
            };
            let submission = self
                .handle
                .submit(
                    principal,
                    envelope.command_id,
                    hash,
                    generation,
                    lease,
                    Some(execute_within),
                    command,
                )
                .await
                .map_err(map_coordinator_error)?;
            let result = self.map_record(&submission.record)?;
            let disposition = if submission.admitted {
                SubmissionDisposition::Accepted
            } else if result.lifecycle().is_terminal() {
                SubmissionDisposition::ExistingTerminal
            } else {
                SubmissionDisposition::ExistingInProgress
            };
            Ok(CommandSubmission {
                result,
                disposition,
            })
        })
    }

    fn command_result<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> ControlFuture<'a, Result<CommandResult, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::DesktopObserve)?;
            if desktop_id != self.desktop_id {
                return Err(ControlPlaneError::NotFound);
            }
            let principal = Self::principal(&context)?;
            let generation = self.token().await?;
            let record = self
                .handle
                .lookup_command(principal, command_id, generation)
                .await
                .map_err(map_coordinator_error)?
                .ok_or(ControlPlaneError::NotFound)?;
            self.map_record(&record)
        })
    }

    fn wait_command<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
        timeout: Duration,
    ) -> ControlFuture<'a, Result<CommandWait, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::DesktopObserve)?;
            if desktop_id != self.desktop_id {
                return Err(ControlPlaneError::NotFound);
            }
            let principal = Self::principal(&context)?;
            let generation = self.token().await?;
            let mut updates = self
                .handle
                .watch_command(principal, command_id, generation)
                .await
                .map_err(map_coordinator_error)?
                .ok_or(ControlPlaneError::NotFound)?;
            let initial = self.map_record(&updates.borrow().clone())?;
            if initial.lifecycle().is_terminal() {
                return Ok(CommandWait::Terminal(initial));
            }
            #[cfg(test)]
            if let Some(wait_started) = &self.wait_started {
                wait_started.add_permits(1);
            }
            let wait = async {
                loop {
                    updates
                        .changed()
                        .await
                        .map_err(|_| ControlPlaneError::Internal)?;
                    let result = self.map_record(&updates.borrow_and_update().clone())?;
                    if result.lifecycle().is_terminal() {
                        return Ok(result);
                    }
                }
            };
            match tokio::time::timeout(timeout, wait).await {
                Ok(result) => result.map(CommandWait::Terminal),
                Err(_) => {
                    let result = self.map_record(&updates.borrow().clone())?;
                    Ok(CommandWait::TimedOut(result))
                }
            }
        })
    }

    fn cancel_command<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> ControlFuture<'a, Result<CommandCancellation, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_cancel_grant(&context)?;
            if desktop_id != self.desktop_id {
                return Err(ControlPlaneError::NotFound);
            }
            let principal = Self::principal(&context)?;
            let generation = self.token().await?;
            let outcome = self
                .handle
                .cancel_command(principal.clone(), command_id, generation)
                .await
                .map_err(map_coordinator_error)?;
            if outcome == CancelCommandOutcome::NotFound {
                return Err(ControlPlaneError::NotFound);
            }
            let record = self
                .handle
                .lookup_command(principal, command_id, generation)
                .await
                .map_err(map_coordinator_error)?
                .ok_or(ControlPlaneError::NotFound)?;
            let result = self.map_record(&record)?;
            Ok(match outcome {
                CancelCommandOutcome::Accepted => CommandCancellation::Accepted(result),
                CancelCommandOutcome::AlreadyTerminal => {
                    CommandCancellation::AlreadyTerminal(result)
                }
                CancelCommandOutcome::NotFound => return Err(ControlPlaneError::NotFound),
            })
        })
    }

    fn subscribe_events<'a>(
        &'a self,
        context: ControlRequestContext,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        since_sequence: Option<u64>,
    ) -> ControlFuture<'a, Result<EventSubscription, ControlPlaneError>> {
        Box::pin(async move {
            Self::require_grant(&context, Grant::DesktopObserve)?;
            if desktop_id != self.desktop_id {
                return Err(ControlPlaneError::NotFound);
            }
            let principal = Self::principal(&context)?;
            let generation = self.token().await?;
            if desktop_generation != generation.generation() {
                return Err(ControlPlaneError::StaleReference {
                    current_generation: Some(generation.generation()),
                });
            }
            let subscription = self
                .handle
                .subscribe_events(generation, since_sequence)
                .await
                .map_err(map_coordinator_error)?;
            let last_global_sequence = match &subscription.replay {
                ReplayResult::Events {
                    latest_sequence, ..
                }
                | ReplayResult::ResyncRequired {
                    latest_sequence, ..
                } => *latest_sequence,
            };
            let replay = match subscription.replay {
                ReplayResult::Events {
                    latest_sequence,
                    events,
                    ..
                } => project_replayed_events(
                    self.desktop_id,
                    &principal,
                    generation,
                    latest_sequence,
                    events,
                )?,
                ReplayResult::ResyncRequired {
                    reason,
                    current_generation,
                    dropped_through,
                    latest_sequence,
                } => EventReplay::ResyncRequired {
                    reason: project_replay_reason(reason),
                    desktop_generation: current_generation.generation(),
                    dropped_through,
                    latest_sequence,
                },
            };
            Ok(EventSubscription {
                replay,
                live: Box::new(RuntimeLiveEventReceiver {
                    live: subscription.live,
                    handle: self.handle.clone(),
                    principal,
                    desktop_id: self.desktop_id,
                    generation,
                    last_global_sequence,
                }),
            })
        })
    }
}

fn lifecycle_for_failure(failure: &RuntimeFailure) -> CommandLifecycle {
    match failure.code {
        ErrorCode::CancelledBeforeEffect => CommandLifecycle::CancelledBeforeEffect,
        ErrorCode::CancelledAfterEffect => CommandLifecycle::CancelledAfterEffect,
        ErrorCode::DeadlineExceededBeforeEffect => CommandLifecycle::DeadlineBeforeEffect,
        ErrorCode::DeadlineExceededAfterEffect => CommandLifecycle::DeadlineAfterEffect,
        _ => CommandLifecycle::Failed,
    }
}

fn terminal_failure(
    terminal: &CommandTerminal<RuntimeResult>,
    _generation: DesktopGeneration,
) -> (CommandLifecycle, RuntimeFailure) {
    let effect_stage = if terminal.effect == CommandEffect::AfterEffect {
        EffectStage::SideEffectObserved
    } else {
        EffectStage::None
    };
    match terminal.cause {
        TerminalCause::Cancelled => {
            let after = terminal.effect == CommandEffect::AfterEffect;
            (
                if after {
                    CommandLifecycle::CancelledAfterEffect
                } else {
                    CommandLifecycle::CancelledBeforeEffect
                },
                RuntimeFailure {
                    status: 409,
                    code: if after {
                        ErrorCode::CancelledAfterEffect
                    } else {
                        ErrorCode::CancelledBeforeEffect
                    },
                    title: "Command cancelled",
                    detail: "The coordinator honored explicit command cancellation.",
                    retry: if after {
                        RetryAdvice::Never
                    } else {
                        RetryAdvice::SameCommandId
                    },
                    effect_stage,
                },
            )
        }
        TerminalCause::DeadlineExceeded => {
            let after = terminal.effect == CommandEffect::AfterEffect;
            (
                if after {
                    CommandLifecycle::DeadlineAfterEffect
                } else {
                    CommandLifecycle::DeadlineBeforeEffect
                },
                RuntimeFailure {
                    status: 504,
                    code: if after {
                        ErrorCode::DeadlineExceededAfterEffect
                    } else {
                        ErrorCode::DeadlineExceededBeforeEffect
                    },
                    title: "Command deadline exceeded",
                    detail: "The coordinator honored the command's monotonic deadline.",
                    retry: if after {
                        RetryAdvice::Never
                    } else {
                        RetryAdvice::SameCommandId
                    },
                    effect_stage,
                },
            )
        }
        TerminalCause::GenerationChanged => (
            CommandLifecycle::Failed,
            RuntimeFailure {
                status: 409,
                code: ErrorCode::StaleReference,
                title: "Desktop generation changed",
                detail: "The command belonged to a retired desktop generation.",
                retry: RetryAdvice::AfterResync,
                effect_stage,
            },
        ),
        TerminalCause::Shutdown => (
            CommandLifecycle::Failed,
            RuntimeFailure {
                status: 503,
                code: ErrorCode::CapabilityUnavailable,
                title: "Daemon is shutting down",
                detail: "The daemon stopped command execution during orderly shutdown.",
                retry: RetryAdvice::AfterBackoff,
                effect_stage,
            },
        ),
        TerminalCause::UnexpectedStop | TerminalCause::ExecutorPanicked => (
            CommandLifecycle::Failed,
            RuntimeFailure {
                status: 500,
                code: ErrorCode::Internal,
                title: "Command execution failed",
                detail: "The command executor stopped without a safe typed result.",
                retry: RetryAdvice::Never,
                effect_stage,
            },
        ),
        TerminalCause::Returned => (
            CommandLifecycle::Failed,
            RuntimeFailure {
                status: 500,
                code: ErrorCode::Internal,
                title: "Command result invalid",
                detail: "The retained command result violated an internal invariant.",
                retry: RetryAdvice::Never,
                effect_stage,
            },
        ),
    }
}

fn runtime_problem(
    failure: &RuntimeFailure,
    generation: DesktopGeneration,
) -> Result<Problem, ControlPlaneError> {
    Problem::new(
        failure.status,
        failure.code,
        failure.title,
        failure.detail,
        failure.retry,
        failure.effect_stage,
    )
    .map(|problem| problem.with_desktop_generation(generation))
    .map_err(|_| ControlPlaneError::Internal)
}

fn canonical_hash(envelope: &CommandEnvelope) -> Result<CanonicalCommandHash, ControlPlaneError> {
    let bytes = serde_json::to_vec(&(
        envelope.protocol_version,
        envelope.desktop_id,
        envelope.desktop_generation,
        &envelope.deadline,
        envelope.trace_policy,
        &envelope.command,
    ))
    .map_err(|_| ControlPlaneError::Internal)?;
    let hash = Sha256::digest(bytes);
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&hash);
    Ok(CanonicalCommandHash::new(digest))
}

fn deadline_duration(
    deadline: Option<&Timestamp>,
    maximum: Duration,
) -> Result<Duration, ControlPlaneError> {
    let Some(deadline) = deadline else {
        return Ok(maximum);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ControlPlaneError::Internal)?
        .as_nanos();
    let now = i128::try_from(now).map_err(|_| ControlPlaneError::Internal)?;
    let remaining = deadline
        .unix_timestamp_nanos()
        .map_err(|_| ControlPlaneError::InvalidRequest)?
        .saturating_sub(now);
    if remaining <= 0 {
        return Ok(Duration::ZERO);
    }
    let nanoseconds = u64::try_from(remaining).unwrap_or(u64::MAX);
    Ok(Duration::from_nanos(nanoseconds).min(maximum))
}

fn map_coordinator_error(error: CoordinatorError) -> ControlPlaneError {
    match error {
        CoordinatorError::CommandCapacityExhausted
        | CoordinatorError::PrincipalCommandCapacityExhausted
        | CoordinatorError::Ledger(CommandLedgerError::CapacityExhausted) => {
            ControlPlaneError::ResourceExhausted
        }
        CoordinatorError::Ledger(CommandLedgerError::CommandIdConflict) => {
            ControlPlaneError::CommandIdConflict
        }
        CoordinatorError::Ledger(CommandLedgerError::UnknownCommand) => ControlPlaneError::NotFound,
        CoordinatorError::Generation(_)
        | CoordinatorError::Ledger(CommandLedgerError::StaleGeneration)
        | CoordinatorError::Lease(LeaseError::StaleGeneration) => {
            ControlPlaneError::StaleReference {
                current_generation: None,
            }
        }
        CoordinatorError::Lease(
            LeaseError::Occupied { .. }
            | LeaseError::WrongOwner
            | LeaseError::WrongLease
            | LeaseError::NotHeld
            | LeaseError::Expired
            | LeaseError::ResetRequired,
        ) => ControlPlaneError::LeaseConflict,
        CoordinatorError::Lease(LeaseError::TtlOutOfRange { .. }) => {
            ControlPlaneError::InvalidRequest
        }
        CoordinatorError::Closed => ControlPlaneError::CapabilityUnavailable,
        _ => ControlPlaneError::Internal,
    }
}

#[derive(Clone, Copy)]
struct ClockProjection {
    unix_origin_ns: i128,
}

impl ClockProjection {
    fn capture() -> Result<Self, CoordinatorSetupError> {
        let nanoseconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CoordinatorSetupError::SystemClock)?
            .as_nanos();
        Ok(Self {
            unix_origin_ns: i128::try_from(nanoseconds)
                .map_err(|_| CoordinatorSetupError::SystemClock)?,
        })
    }

    fn timestamp(self, monotonic: MonotonicMillis) -> Result<Timestamp, ControlPlaneError> {
        let delta = i128::from(monotonic.get())
            .checked_mul(1_000_000)
            .ok_or(ControlPlaneError::Internal)?;
        let value = self
            .unix_origin_ns
            .checked_add(delta)
            .ok_or(ControlPlaneError::Internal)?;
        Timestamp::from_unix_timestamp_nanos(value).map_err(|_| ControlPlaneError::Internal)
    }
}

/// Coordinator composition failure before the HTTP listener starts.
#[derive(Debug, Error)]
pub(crate) enum CoordinatorSetupError {
    #[error("system clock cannot be projected into protocol timestamps")]
    SystemClock,
    #[error("configured duration overflowed milliseconds")]
    DurationOverflow,
    #[error("built-in event wire definitions are invalid")]
    EventWire,
    #[error("clipboard runtime belongs to another desktop lifetime")]
    ClipboardScope,
    #[error(transparent)]
    Motion(#[from] MotionPlanError),
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error(transparent)]
    Ledger(#[from] CommandLedgerError),
    #[error(transparent)]
    Event(#[from] EventHubError),
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
}

/// Coordinator shutdown or task-ownership failure.
#[derive(Debug, Error)]
pub(crate) enum CoordinatorRuntimeError {
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error("coordinator task panicked")]
    TaskPanicked,
    #[error("process event relay task panicked")]
    ProcessEventTaskPanicked,
    #[error("external desktop event relay task panicked")]
    ExternalEventTaskPanicked,
    #[error(transparent)]
    ProcessEvent(#[from] ProcessEventRelayError),
}

/// Internal process-event normalization or coordinator publication failure.
#[derive(Debug, Error)]
pub(crate) enum ProcessEventRelayError {
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    #[error("process event belongs to another desktop generation")]
    WrongGeneration,
    #[error("process event owner is invalid")]
    InvalidPrincipal,
    #[error("process event payload is invalid")]
    InvalidEvent,
    #[error("shared desktop event ingress closed unexpectedly")]
    EventIngressClosed,
}

/// Nonblocking actor-to-coordinator event publication failure.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ExternalEventIngressError {
    /// The supplied topic or payload exceeded normalized event limits.
    #[error("external desktop event is invalid")]
    InvalidEvent,
    /// The bounded event queue was full and a resync barrier was latched.
    #[error("external desktop event queue is full; resync required")]
    Full,
    /// The coordinator event relay has closed.
    #[error("external desktop event relay is closed")]
    Closed,
}

#[cfg(test)]
#[path = "control_plane_window_tests.rs"]
mod window_control_tests;

#[cfg(test)]
#[path = "control_plane_input_tests.rs"]
mod public_input_tests;

#[cfg(test)]
#[path = "control_plane_clipboard_tests.rs"]
mod clipboard_command_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tokio::sync::Semaphore;
    use tower::ServiceExt;
    use xenoteer_protocol::{
        DesktopProbeCommand, LaunchId, ProcessRef, ProcessStatusCommand, RequestId,
    };
    use xenoteer_server::{
        AllowedOrigins, Authentication, DesktopReadiness, Principal, ReadinessHandle,
        ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
        api_router_with_control,
    };

    use super::*;

    #[test]
    fn principal_event_filter_hides_payloads_without_renumbering_global_gaps()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let alice = PrincipalId::new("alice")?;
        let bob = PrincipalId::new("bob")?;
        let make = |audience: PrincipalId, sequence: u64, secret: &str| {
            Ok::<_, Box<dyn std::error::Error>>(EventRecord {
                generation,
                sequence,
                encoded_size: 128,
                event: RuntimeEvent::Targeted {
                    audience,
                    event: NormalizedEvent::new(
                        EventTopic::new("command.lifecycle")?,
                        serde_json::json!({"value": secret}),
                    )?,
                },
            })
        };
        let records = vec![
            make(alice.clone(), 1, "alice-one")?,
            make(bob, 2, "bob-secret")?,
            make(alice.clone(), 3, "alice-three")?,
        ];
        let visible = records
            .into_iter()
            .map(|record| project_event(desktop_id, &alice, record))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| std::io::Error::other("event projection failed"))?
            .into_iter()
            .filter_map(|event| match event {
                ProjectedEvent::Visible(event) => Some(event),
                ProjectedEvent::Hidden | ProjectedEvent::ResyncBarrier { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            visible
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(
            visible
                .iter()
                .all(|event| event.payload["value"] != "bob-secret")
        );
        Ok(())
    }

    #[test]
    fn observer_broadcast_projects_once_for_every_principal()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let record = EventRecord {
            generation,
            sequence: 9,
            encoded_size: 128,
            event: RuntimeEvent::Broadcast {
                event: NormalizedEvent::new(
                    EventTopic::new("window.changed")?,
                    serde_json::json!({"model_revision": 17}),
                )?,
            },
        };
        for principal in [PrincipalId::new("alice")?, PrincipalId::new("bob")?] {
            let projected = project_event(desktop_id, &principal, record.clone())
                .map_err(|_| std::io::Error::other("event projection failed"))?;
            assert!(matches!(
                projected,
                ProjectedEvent::Visible(SequencedEvent { sequence: 9, .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn full_external_event_queue_latches_resync_without_blocking()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, _receiver) = mpsc::channel(1);
        let resync_state = Arc::new(AtomicU64::new(0));
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::clone(&resync_state),
            resync_notify: Arc::new(Notify::new()),
        };
        let topic = EventTopic::new("window.changed")?;
        ingress.try_broadcast(topic.clone(), serde_json::json!({"model_revision": 1}))?;
        assert_eq!(
            ingress.try_broadcast(topic, serde_json::json!({"model_revision": 2})),
            Err(ExternalEventIngressError::Full)
        );
        assert_eq!(resync_state.load(Ordering::Acquire), 1);
        Ok(())
    }

    #[test]
    fn epoch_admission_closes_for_gap_and_reopens_after_barrier_claim()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, mut receiver) = mpsc::channel(4);
        let resync_state = Arc::new(AtomicU64::new(0));
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::clone(&resync_state),
            resync_notify: Arc::new(Notify::new()),
        };
        let topic = EventTopic::new("window.changed")?;

        ingress.require_resync();
        ingress.require_resync();
        assert_eq!(resync_state.load(Ordering::Acquire), 1);
        assert_eq!(
            ingress.try_broadcast(topic.clone(), serde_json::json!({"model_revision": 1})),
            Err(ExternalEventIngressError::Full)
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(claim_external_resync(&resync_state), Some(2));
        assert_eq!(claim_external_resync(&resync_state), None);

        ingress.try_broadcast(topic, serde_json::json!({"model_revision": 2}))?;
        let queued = receiver.try_recv()?;
        assert_eq!(queued.admission_epoch, 2);
        assert!(matches!(
            event_after_external_barrier(queued, 2),
            Some(RuntimeEvent::Broadcast { .. })
        ));
        Ok(())
    }

    #[test]
    fn forced_pre_gap_admission_cannot_publish_after_claimed_barrier()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, mut receiver) = mpsc::channel(4);
        let resync_state = Arc::new(AtomicU64::new(0));
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::clone(&resync_state),
            resync_notify: Arc::new(Notify::new()),
        };
        let event = || -> Result<RuntimeEvent, Box<dyn std::error::Error>> {
            Ok(RuntimeEvent::Broadcast {
                event: NormalizedEvent::new(
                    EventTopic::new("window.changed")?,
                    serde_json::json!({"model_revision": 7}),
                )?,
            })
        };

        // Force load-before-gap / enqueue-after-claim: the queue entry retains
        // epoch zero and is rejected after the epoch-two barrier.
        let delayed = EpochRuntimeEvent {
            admission_epoch: resync_state.load(Ordering::Acquire),
            event: event()?,
        };
        ingress.require_resync();
        assert_eq!(claim_external_resync(&resync_state), Some(2));
        assert!(ingress.sender.try_send(delayed).is_ok());
        assert!(event_after_external_barrier(receiver.try_recv()?, 2).is_none());

        // Force select-before-gap / publish-check-after-gap: the event that
        // was already selected is likewise older than the claimed barrier.
        ingress.try_send(event()?)?;
        let selected = receiver.try_recv()?;
        assert_eq!(selected.admission_epoch, 2);
        ingress.require_resync();
        assert_eq!(claim_external_resync(&resync_state), Some(4));
        assert!(event_after_external_barrier(selected, 4).is_none());

        ingress.try_send(event()?)?;
        assert!(matches!(
            event_after_external_barrier(receiver.try_recv()?, 4),
            Some(RuntimeEvent::Broadcast { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn process_gap_invalidates_events_admitted_through_the_shared_epoch()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, mut receiver) = mpsc::channel(4);
        let resync_state = Arc::new(AtomicU64::new(0));
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::clone(&resync_state),
            resync_notify: Arc::new(Notify::new()),
        };
        ingress.try_broadcast(
            EventTopic::new("window.changed")?,
            serde_json::json!({"model_revision": 7}),
        )?;

        let generation = DesktopGeneration::new();
        let process_event: BrokerProcessEvent = serde_json::from_value(serde_json::json!({
            "sequence": 2,
            "principal_id": "alice",
            "payload": process_exit_payload(generation, false, false)?,
        }))?;
        let cursor = relay_process_event(
            &ingress,
            generation,
            &CancellationToken::new(),
            0,
            process_event,
        )
        .await?;
        assert_eq!(cursor, 2);
        assert_eq!(resync_state.load(Ordering::Acquire), 1);

        let claimed_epoch = claim_external_resync(&resync_state).ok_or("gap was not claimed")?;
        assert_eq!(claimed_epoch, 2);
        assert!(event_after_external_barrier(receiver.try_recv()?, claimed_epoch).is_none());
        assert!(receiver.try_recv().is_err());

        ingress.try_broadcast(
            EventTopic::new("window.changed")?,
            serde_json::json!({"model_revision": 8}),
        )?;
        assert!(matches!(
            event_after_external_barrier(receiver.try_recv()?, claimed_epoch),
            Some(RuntimeEvent::Broadcast { .. })
        ));
        Ok(())
    }

    #[test]
    fn closed_shared_epoch_is_fatal_unless_process_relay_is_cancelling()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, receiver) = mpsc::channel(1);
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::new(AtomicU64::new(1)),
            resync_notify: Arc::new(Notify::new()),
        };
        drop(receiver);
        let event = || -> Result<RuntimeEvent, Box<dyn std::error::Error>> {
            Ok(RuntimeEvent::Broadcast {
                event: NormalizedEvent::new(
                    EventTopic::new("window.changed")?,
                    serde_json::json!({"model_revision": 9}),
                )?,
            })
        };
        let cancellation = CancellationToken::new();

        assert!(matches!(
            publish_process_event(&ingress, &cancellation, event()?),
            Err(ProcessEventRelayError::EventIngressClosed)
        ));
        assert!(matches!(
            require_process_resync(&ingress, &cancellation),
            Err(ProcessEventRelayError::EventIngressClosed)
        ));

        cancellation.cancel();
        publish_process_event(&ingress, &cancellation, event()?)?;
        require_process_resync(&ingress, &cancellation)?;
        Ok(())
    }

    #[test]
    fn natural_and_requested_process_exits_normalize_only_for_the_owner()
    -> Result<(), Box<dyn std::error::Error>> {
        let generation = DesktopGeneration::new();
        for (termination_requested, forced_escalation) in [(false, false), (true, true)] {
            let payload =
                process_exit_payload(generation, termination_requested, forced_escalation)?;
            let normalized = normalize_process_payload(generation, "alice".to_owned(), payload)?;
            let RuntimeEvent::Targeted { audience, event } = normalized else {
                return Err("process exit became a resync barrier".into());
            };
            assert_eq!(audience, PrincipalId::new("alice")?);
            assert_eq!(event.topic.as_str(), PROCESS_EXITED_TOPIC);
            let payload: ProcessExitedEvent = serde_json::from_value(event.payload)?;
            assert_eq!(payload.termination_requested, termination_requested);
            assert_eq!(payload.forced_escalation, forced_escalation);
        }
        Ok(())
    }

    #[test]
    fn invalid_or_wrong_generation_process_events_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let generation = DesktopGeneration::new();
        let wrong = process_exit_payload(DesktopGeneration::new(), false, false)?;
        assert!(matches!(
            normalize_process_payload(generation, "alice".to_owned(), wrong),
            Err(ProcessEventRelayError::WrongGeneration)
        ));

        let mut malformed = process_exit_payload(generation, false, false)?;
        malformed.process.state = xenoteer_protocol::ProcessState::Running;
        malformed.process.exit = None;
        assert!(matches!(
            normalize_process_payload(generation, "alice".to_owned(), malformed),
            Err(ProcessEventRelayError::InvalidEvent)
        ));
        assert!(matches!(
            normalize_process_payload(
                generation,
                "bad owner".to_owned(),
                process_exit_payload(generation, false, false)?,
            ),
            Err(ProcessEventRelayError::InvalidPrincipal)
        ));
        Ok(())
    }

    #[test]
    fn retained_resync_barrier_forces_every_principal_to_resynchronize()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let replay = project_replayed_events(
            desktop_id,
            &PrincipalId::new("unrelated-observer")?,
            generation,
            3,
            vec![EventRecord {
                generation,
                sequence: 2,
                encoded_size: RESYNC_BARRIER_RETENTION_CHARGE,
                event: RuntimeEvent::ResyncBarrier,
            }],
        )
        .map_err(|_| std::io::Error::other("resync barrier projection failed"))?;
        assert!(matches!(
            replay,
            EventReplay::ResyncRequired {
                reason: EventResyncReason::HistoryLost,
                dropped_through: 2,
                latest_sequence: 3,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn process_event_reconnect_wait_observes_shutdown_immediately()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            wait_for_process_event_reconnect(&cancellation, Duration::from_secs(60)),
        )
        .await?;
        assert!(cancelled);
        Ok(())
    }

    fn process_exit_payload(
        generation: DesktopGeneration,
        termination_requested: bool,
        forced_escalation: bool,
    ) -> Result<ProcessExitedEvent, Box<dyn std::error::Error>> {
        Ok(ProcessExitedEvent {
            application: xenoteer_protocol::ApplicationId::new("fixture")?,
            process: xenoteer_protocol::ProcessView {
                process: ProcessRef {
                    desktop_generation: generation,
                    pid: 42,
                    proc_start_ticks: 7,
                    launch_id: LaunchId::new(),
                },
                state: xenoteer_protocol::ProcessState::Exited,
                exit: Some(xenoteer_protocol::ProcessExit {
                    code: Some(0),
                    signal: None,
                    core_dumped: false,
                }),
            },
            termination_requested,
            forced_escalation,
        })
    }

    #[test]
    fn runtime_mapper_emits_one_central_event_for_each_actor_transition()
    -> Result<(), Box<dyn std::error::Error>> {
        let mapper = RuntimeEventMapper::new()?;
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let principal = PrincipalId::new("alice")?;
        let base = CommandRecord {
            principal: principal.clone(),
            command_id: CommandId::new(),
            generation,
            request_hash: CanonicalCommandHash::new([4; 32]),
            state: CommandRecordState::Accepted,
            accepted_at: MonotonicMillis::new(1),
            updated_at: MonotonicMillis::new(1),
        };
        let accepted = mapper.map_command_transition(&principal, &base);
        let mut running_record = base.clone();
        running_record.state = CommandRecordState::Running;
        running_record.updated_at = MonotonicMillis::new(2);
        let running = mapper.map_command_transition(&principal, &running_record);
        let mut terminal_record = running_record;
        terminal_record.state = CommandRecordState::Terminal(CommandTerminal {
            cause: TerminalCause::Returned,
            effect: CommandEffect::BeforeEffect,
            output: Some(RuntimeResult::success(
                CommandOutcome::Acknowledged,
                EffectStage::None,
            )),
        });
        terminal_record.updated_at = MonotonicMillis::new(3);
        let terminal = mapper.map_command_transition(&principal, &terminal_record);

        assert_eq!(accepted.len(), 1);
        assert_eq!(running.len(), 1);
        assert_eq!(terminal.len(), 1);
        assert!(matches!(
            &accepted[0].event,
            RuntimeEvent::Targeted { event, .. }
                if event.topic.as_str() == COMMAND_LIFECYCLE_TOPIC
        ));
        assert!(matches!(
            &running[0].event,
            RuntimeEvent::Targeted { event, .. }
                if event.topic.as_str() == ACTION_LIFECYCLE_TOPIC
        ));
        assert!(matches!(
            &terminal[0].event,
            RuntimeEvent::Targeted { event, .. }
                if event.topic.as_str() == COMMAND_LIFECYCLE_TOPIC
        ));
        Ok(())
    }

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[derive(Clone, Copy)]
    enum StagedMode {
        TwoStageSuccess,
        CancelBeforeEffect,
    }

    #[derive(Clone)]
    struct StagedRuntimeExecutor {
        mode: StagedMode,
        executions: Arc<AtomicUsize>,
        effects: Arc<AtomicUsize>,
        started: Arc<Semaphore>,
        allow_effect: Arc<Semaphore>,
        effect_observed: Arc<Semaphore>,
        allow_result: Arc<Semaphore>,
        stop_observed: Arc<Semaphore>,
    }

    impl StagedRuntimeExecutor {
        fn new(mode: StagedMode) -> Self {
            Self {
                mode,
                executions: Arc::new(AtomicUsize::new(0)),
                effects: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Semaphore::new(0)),
                allow_effect: Arc::new(Semaphore::new(0)),
                effect_observed: Arc::new(Semaphore::new(0)),
                allow_result: Arc::new(Semaphore::new(0)),
                stop_observed: Arc::new(Semaphore::new(0)),
            }
        }
    }

    impl CommandExecutor<RuntimeCommand, RuntimeResult> for StagedRuntimeExecutor {
        fn execute(
            &self,
            command: RuntimeCommand,
            mut context: ExecutionContext,
        ) -> xenoteer_core::coordinator::BoxCoordinatorFuture<ExecutionOutcome<RuntimeResult>>
        {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            let mode = self.mode;
            let effects = Arc::clone(&self.effects);
            let allow_effect = Arc::clone(&self.allow_effect);
            let effect_observed = Arc::clone(&self.effect_observed);
            let allow_result = Arc::clone(&self.allow_result);
            let stop_observed = Arc::clone(&self.stop_observed);
            Box::pin(async move {
                let _stable_command_id = command.command_id;
                let _stable_principal = command.principal;
                if !matches!(command.command, Command::DesktopProbe(_)) {
                    return completed(invalid_input());
                }
                match mode {
                    StagedMode::TwoStageSuccess => {
                        if let Ok(permit) = allow_effect.acquire().await {
                            permit.forget();
                        }
                        effects.fetch_add(1, Ordering::SeqCst);
                        effect_observed.add_permits(1);
                        if let Ok(permit) = allow_result.acquire().await {
                            permit.forget();
                        }
                        completed(RuntimeResult::success(
                            CommandOutcome::Probe { ready: true },
                            EffectStage::PostconditionMet,
                        ))
                    }
                    StagedMode::CancelBeforeEffect => {
                        let _reason = context.wait_for_stop().await;
                        stop_observed.add_permits(1);
                        ExecutionOutcome::Stopped {
                            effect: CommandEffect::BeforeEffect,
                        }
                    }
                }
            })
        }

        fn reset_owned_input(
            &self,
            _request: ResetRequest,
        ) -> xenoteer_core::coordinator::BoxCoordinatorFuture<ResetOutcome> {
            Box::pin(async { ResetOutcome::Complete })
        }
    }

    struct StagedTransportFixture {
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        application: axum::Router,
        runtime: CoordinatorRuntime,
        executor: StagedRuntimeExecutor,
        wait_started: Arc<Semaphore>,
    }

    impl StagedTransportFixture {
        fn new(mode: StagedMode) -> Result<Self, Box<dyn std::error::Error>> {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let config = Config::default();
            let limits = config.limits();
            let active_global = limits.accepted_commands_per_daemon();
            let settings = CoordinatorSettings::new(
                desktop_id,
                generation,
                active_global,
                active_global,
                limits.accepted_commands_per_principal(),
                active_global.min(MAX_CONCURRENT_EXECUTIONS),
                LeasePolicy::new(LEASE_TTL_MS, LEASE_TTL_MS)?,
                CommandLedgerLimits::new(
                    limits.result_ledger_entries(),
                    limits.result_ledger_ttl_seconds().saturating_mul(1_000),
                )?,
                EventHubLimits::new(EVENT_RETENTION_COUNT, EVENT_RETENTION_BYTES)?,
            )?;
            let executor = StagedRuntimeExecutor::new(mode);
            let event_mapper = RuntimeEventMapper::new()?;
            let (handle, join) =
                spawn_coordinator_with_event_mapper(settings, executor.clone(), event_mapper)?;
            let wait_started = Arc::new(Semaphore::new(0));
            let control = Arc::new(CoordinatorControlPlane {
                handle: handle.clone(),
                desktop_id,
                generation,
                default_timeout: Duration::from_millis(limits.default_action_timeout_ms()),
                clock: ClockProjection::capture()?,
                wait_started: Some(Arc::clone(&wait_started)),
            });
            let process_event_cancellation = CancellationToken::new();
            let (external_event_ingress, external_event_join) = spawn_external_event_relay(
                handle.clone(),
                generation,
                process_event_cancellation.clone(),
            );
            let runtime = CoordinatorRuntime {
                handle,
                join,
                process_event_cancellation,
                process_event_join: tokio::spawn(async { Ok(()) }),
                external_event_ingress,
                external_event_join,
                control,
            };
            let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
                DesktopReadiness::Ready,
                Some(generation),
                None::<String>,
            ));
            let provider = StaticTokenProvider::single(TOKEN, Principal::local_operator()?)?;
            let application = api_router_with_control(
                readiness,
                desktop_id,
                Authentication::bearer(provider),
                StaticCapabilityProvider::empty()?,
                TransportLimits::default(),
                AllowedOrigins::default(),
                runtime.control(),
            );
            Ok(Self {
                desktop_id,
                generation,
                application,
                runtime,
                executor,
                wait_started,
            })
        }

        fn envelope(
            &self,
            command_id: CommandId,
        ) -> Result<CommandEnvelope, Box<dyn std::error::Error>> {
            Ok(CommandEnvelope::new(
                xenoteer_protocol::ProtocolVersion::V1_0,
                RequestId::new(),
                command_id,
                self.desktop_id,
                self.generation,
                Command::DesktopProbe(DesktopProbeCommand {}),
            )?)
        }

        async fn shutdown(self) -> Result<(), CoordinatorRuntimeError> {
            self.runtime.shutdown().await
        }
    }

    #[test]
    fn canonical_hash_ignores_transport_and_capability_ids_but_not_behavior()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let command = Command::PointerMove(xenoteer_protocol::PointerMoveCommand {
            target: xenoteer_protocol::Point::new(10, 20),
            duration_ms: Some(50),
            curve: PointerCurve::Smooth,
        });
        let first = CommandEnvelope::new_with_lease(
            xenoteer_protocol::ProtocolVersion::V1_0,
            RequestId::new(),
            command_id,
            desktop_id,
            generation,
            ControlLeaseId::new(),
            command.clone(),
        )?;
        let second = CommandEnvelope::new_with_lease(
            xenoteer_protocol::ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            ControlLeaseId::new(),
            command,
        )?;
        assert_eq!(test_hash(&first)?, test_hash(&second)?);

        let mut changed = second;
        changed.command = Command::PointerMove(xenoteer_protocol::PointerMoveCommand {
            target: xenoteer_protocol::Point::new(11, 20),
            duration_ms: Some(50),
            curve: PointerCurve::Smooth,
        });
        assert_ne!(test_hash(&first)?, test_hash(&changed)?);
        Ok(())
    }

    #[test]
    fn ambiguous_process_failures_are_conservative_only_for_mutations() {
        let launch = process_rejection(BrokerErrorCode::Internal, ProcessOperation::Launch);
        let status = process_rejection(BrokerErrorCode::Internal, ProcessOperation::Status);
        assert_eq!(launch.effect_stage(), EffectStage::SideEffectObserved);
        assert_eq!(status.effect_stage(), EffectStage::None);
        assert!(process_error_may_have_effect(
            &BrokerClientError::UnexpectedReply,
            ProcessOperation::Terminate,
        ));
        assert!(!process_error_may_have_effect(
            &BrokerClientError::Connect(std::io::Error::other("offline")),
            ProcessOperation::Launch,
        ));
    }

    #[test]
    fn successful_process_mutations_are_identity_bearing_atomic_completions() {
        let process = ProcessRef {
            desktop_generation: DesktopGeneration::new(),
            pid: 4_244,
            proc_start_ticks: 101,
            launch_id: LaunchId::new(),
        };
        assert!(matches!(
            process_completion(Ok(process), ProcessOperation::Launch),
            ExecutionOutcome::AtomicCompleted {
                output: RuntimeResult::Success(RuntimeSuccess {
                    outcome: CommandOutcome::ApplicationLaunched { process: returned },
                    ..
                }),
                ..
            } if returned == process
        ));

        let view = xenoteer_protocol::ProcessView {
            process,
            state: xenoteer_protocol::ProcessState::Exited,
            exit: Some(xenoteer_protocol::ProcessExit {
                code: Some(0),
                signal: None,
                core_dumped: false,
            }),
        };
        assert!(matches!(
            process_completion(Ok(view.clone()), ProcessOperation::Terminate),
            ExecutionOutcome::AtomicCompleted {
                output: RuntimeResult::Success(RuntimeSuccess {
                    outcome: CommandOutcome::ProcessTerminated { process: returned },
                    ..
                }),
                ..
            } if returned == view
        ));
        assert!(matches!(
            process_completion(Ok(view), ProcessOperation::Status),
            ExecutionOutcome::Completed { .. }
        ));
    }

    #[tokio::test]
    async fn rest_lost_acceptance_response_exact_retry_executes_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::TwoStageSuccess)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;

        let lost = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(lost.status(), StatusCode::ACCEPTED);
        drop(lost);
        fixture.executor.started.acquire().await?.forget();
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 0);

        envelope.request_id = RequestId::new();
        let retry = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry.status(), StatusCode::ACCEPTED);
        drop(retry);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 0);

        fixture.executor.allow_effect.add_permits(1);
        fixture.executor.effect_observed.acquire().await?.forget();
        fixture.executor.allow_result.add_permits(1);
        let terminal = wait_result(&fixture, command_id).await?;
        assert_eq!(terminal.lifecycle(), CommandLifecycle::Succeeded);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 1);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_dropped_wait_after_effect_preserves_one_immutable_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::TwoStageSuccess)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;
        let accepted = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        drop(accepted);
        fixture.executor.started.acquire().await?.forget();
        fixture.executor.allow_effect.add_permits(1);
        fixture.executor.effect_observed.acquire().await?.forget();
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 1);

        let wait_application = fixture.application.clone();
        let wait_request = wait_request(fixture.desktop_id, command_id)?;
        let dropped_wait =
            tokio::spawn(async move { wait_application.oneshot(wait_request).await });
        fixture.wait_started.acquire().await?.forget();
        dropped_wait.abort();
        assert!(matches!(
            dropped_wait.await,
            Err(error) if error.is_cancelled()
        ));

        fixture.executor.allow_result.add_permits(1);
        let terminal = wait_result(&fixture, command_id).await?;
        envelope.request_id = RequestId::new();
        let retry_response = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry_response.status(), StatusCode::OK);
        let retry = response_result(retry_response).await?;
        assert_eq!(retry, terminal);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 1);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_lost_terminal_response_is_recovered_by_get_and_exact_retry()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::TwoStageSuccess)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;
        let accepted = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        drop(accepted);
        fixture.executor.started.acquire().await?.forget();
        fixture.executor.allow_effect.add_permits(1);
        fixture.executor.effect_observed.acquire().await?.forget();
        fixture.executor.allow_result.add_permits(1);

        let lost_terminal = fixture
            .application
            .clone()
            .oneshot(wait_request(fixture.desktop_id, command_id)?)
            .await?;
        assert_eq!(lost_terminal.status(), StatusCode::OK);
        drop(lost_terminal);

        let get_response = fixture
            .application
            .clone()
            .oneshot(get_request(fixture.desktop_id, command_id)?)
            .await?;
        assert_eq!(get_response.status(), StatusCode::OK);
        let recovered = response_result(get_response).await?;
        envelope.request_id = RequestId::new();
        let retry_response = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry_response.status(), StatusCode::OK);
        assert_eq!(response_result(retry_response).await?, recovered);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 1);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_expired_public_deadline_is_immutable_before_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::TwoStageSuccess)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let past = i128::try_from(now)?.saturating_sub(1_000_000_000);
        envelope.deadline = Some(Timestamp::from_unix_timestamp_nanos(past)?);
        let accepted = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        drop(accepted);

        let terminal = wait_result(&fixture, command_id).await?;
        assert_eq!(terminal.lifecycle(), CommandLifecycle::DeadlineBeforeEffect);
        assert_eq!(terminal.effect_stage(), EffectStage::None);
        assert!(matches!(
            terminal.error(),
            Some(problem) if problem.code() == ErrorCode::DeadlineExceededBeforeEffect
        ));
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 0);

        envelope.request_id = RequestId::new();
        let retry_response = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry_response.status(), StatusCode::OK);
        assert_eq!(response_result(retry_response).await?, terminal);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 0);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_cancel_after_effect_overrides_non_atomic_success_and_is_immutable()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::TwoStageSuccess)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;
        let accepted = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        drop(accepted);
        fixture.executor.started.acquire().await?.forget();
        fixture.executor.allow_effect.add_permits(1);
        fixture.executor.effect_observed.acquire().await?.forget();

        let cancellation = fixture
            .application
            .clone()
            .oneshot(delete_request(fixture.desktop_id, command_id)?)
            .await?;
        assert_eq!(cancellation.status(), StatusCode::ACCEPTED);
        drop(cancellation);
        fixture.executor.allow_result.add_permits(1);

        let terminal = wait_result(&fixture, command_id).await?;
        assert_eq!(terminal.lifecycle(), CommandLifecycle::CancelledAfterEffect);
        assert_eq!(terminal.effect_stage(), EffectStage::SideEffectObserved);
        assert!(matches!(
            terminal.error(),
            Some(problem) if problem.code() == ErrorCode::CancelledAfterEffect
        ));

        envelope.request_id = RequestId::new();
        let retry_response = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry_response.status(), StatusCode::OK);
        assert_eq!(response_result(retry_response).await?, terminal);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 1);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_delete_running_cancellation_and_retries_are_immutable()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = StagedTransportFixture::new(StagedMode::CancelBeforeEffect)?;
        let command_id = CommandId::new();
        let mut envelope = fixture.envelope(command_id)?;
        let accepted = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        drop(accepted);
        fixture.executor.started.acquire().await?.forget();

        let cancellation = fixture
            .application
            .clone()
            .oneshot(delete_request(fixture.desktop_id, command_id)?)
            .await?;
        assert_eq!(cancellation.status(), StatusCode::ACCEPTED);
        drop(cancellation);
        fixture.executor.stop_observed.acquire().await?.forget();
        let terminal = wait_result(&fixture, command_id).await?;
        assert_eq!(
            terminal.lifecycle(),
            CommandLifecycle::CancelledBeforeEffect
        );
        assert_eq!(terminal.effect_stage(), EffectStage::None);

        let repeated_delete = fixture
            .application
            .clone()
            .oneshot(delete_request(fixture.desktop_id, command_id)?)
            .await?;
        assert_eq!(repeated_delete.status(), StatusCode::OK);
        assert_eq!(response_result(repeated_delete).await?, terminal);
        envelope.request_id = RequestId::new();
        let retry_response = fixture
            .application
            .clone()
            .oneshot(command_request(fixture.desktop_id, &envelope)?)
            .await?;
        assert_eq!(retry_response.status(), StatusCode::OK);
        assert_eq!(response_result(retry_response).await?, terminal);
        assert_eq!(fixture.executor.executions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.executor.effects.load(Ordering::SeqCst), 0);
        fixture.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn rest_submission_wait_dedupe_and_conflict_use_one_real_coordinator()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let (_input_tx, input_rx) = watch::channel(None::<InputActorHandle>);
        let runtime = spawn(&Config::default(), desktop_id, generation, input_rx)?;
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let principal = Principal::local_operator()?;
        let provider = StaticTokenProvider::single(TOKEN, principal)?;
        let application = api_router_with_control(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
            runtime.control(),
        );
        let command_id = CommandId::new();
        let mut envelope = CommandEnvelope::new(
            xenoteer_protocol::ProtocolVersion::V1_0,
            RequestId::new(),
            command_id,
            desktop_id,
            generation,
            Command::DesktopProbe(DesktopProbeCommand {}),
        )?;

        let accepted = application
            .clone()
            .oneshot(command_request(desktop_id, &envelope)?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);

        let terminal = application
            .clone()
            .oneshot(
                Request::get(format!(
                    "/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=1000"
                ))
                .header(header::AUTHORIZATION, authorization())
                .body(Body::empty())?,
            )
            .await?;
        assert_eq!(terminal.status(), StatusCode::OK);
        let body = to_bytes(terminal.into_body(), 16 * 1_024).await?;
        let result: CommandResult = serde_json::from_slice(&body)?;
        assert_eq!(result.lifecycle(), CommandLifecycle::Succeeded);

        envelope.request_id = RequestId::new();
        let duplicate = application
            .clone()
            .oneshot(command_request(desktop_id, &envelope)?)
            .await?;
        assert_eq!(duplicate.status(), StatusCode::OK);

        let conflict = CommandEnvelope::new(
            xenoteer_protocol::ProtocolVersion::V1_0,
            RequestId::new(),
            command_id,
            desktop_id,
            generation,
            Command::ProcessStatus(ProcessStatusCommand {
                process: ProcessRef {
                    desktop_generation: generation,
                    pid: 1,
                    proc_start_ticks: 1,
                    launch_id: LaunchId::new(),
                },
            }),
        )?;
        let conflict = application
            .oneshot(command_request(desktop_id, &conflict)?)
            .await?;
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        runtime.shutdown().await?;
        Ok(())
    }

    fn command_request(
        desktop_id: DesktopId,
        envelope: &CommandEnvelope,
    ) -> Result<Request<Body>, Box<dyn std::error::Error>> {
        Ok(Request::post(format!("/v1/desktops/{desktop_id}/commands"))
            .header(header::AUTHORIZATION, authorization())
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", envelope.command_id.to_string())
            .body(Body::from(serde_json::to_vec(envelope)?))?)
    }

    fn get_request(
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> Result<Request<Body>, Box<dyn std::error::Error>> {
        Ok(
            Request::get(format!("/v1/desktops/{desktop_id}/commands/{command_id}"))
                .header(header::AUTHORIZATION, authorization())
                .body(Body::empty())?,
        )
    }

    fn wait_request(
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> Result<Request<Body>, Box<dyn std::error::Error>> {
        Ok(Request::get(format!(
            "/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=1000"
        ))
        .header(header::AUTHORIZATION, authorization())
        .body(Body::empty())?)
    }

    fn delete_request(
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> Result<Request<Body>, Box<dyn std::error::Error>> {
        Ok(
            Request::delete(format!("/v1/desktops/{desktop_id}/commands/{command_id}"))
                .header(header::AUTHORIZATION, authorization())
                .body(Body::empty())?,
        )
    }

    async fn response_result(
        response: axum::response::Response,
    ) -> Result<CommandResult, Box<dyn std::error::Error>> {
        let body = to_bytes(response.into_body(), 16 * 1_024).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    async fn wait_result(
        fixture: &StagedTransportFixture,
        command_id: CommandId,
    ) -> Result<CommandResult, Box<dyn std::error::Error>> {
        let response = fixture
            .application
            .clone()
            .oneshot(wait_request(fixture.desktop_id, command_id)?)
            .await?;
        if response.status() != StatusCode::OK {
            return Err(std::io::Error::other("command did not reach terminal state").into());
        }
        response_result(response).await
    }

    fn authorization() -> String {
        format!(
            "Bearer {}",
            std::str::from_utf8(TOKEN).unwrap_or("invalid-test-token")
        )
    }

    fn test_hash(envelope: &CommandEnvelope) -> Result<CanonicalCommandHash, std::io::Error> {
        canonical_hash(envelope)
            .map_err(|_| std::io::Error::other("canonical hash construction failed"))
    }
}
