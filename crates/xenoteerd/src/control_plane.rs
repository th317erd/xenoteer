//! Daemon composition for the coordinator, input actor, and process broker.

use std::{
    fmt::Write as _,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
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
use xenoteer_atspi::SemanticRect;
use xenoteer_core::{
    AccessibilityCorrelationLimits, Config, ElementClickPlanError, PhysicalElementClickPlan,
    RevalidatedElementClick,
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
    plan_physical_element_click, revalidate_physical_element_click,
};
use xenoteer_processd::{
    BrokerClient, BrokerClientError, BrokerErrorCode, BrokerEventReplay, BrokerEventStream,
    BrokerLiveEvent, BrokerProcessEvent, DEFAULT_BROKER_SOCKET,
};
use xenoteer_protocol::{
    ACTION_LIFECYCLE_TOPIC, ArtifactRef, COMMAND_LIFECYCLE_TOPIC, ClipboardPasteEvidence,
    ClipboardRestorationEvidence, ClipboardRestorationKind, ClipboardTarget, ClipboardWriteSource,
    Command, CommandEnvelope, CommandId, CommandLifecycle, CommandOutcome, CommandResult,
    CommandTrace, CommandTraceDomain, CommandTraceStage, CommandTraceStatus, CommandTraceStep,
    ControlLeaseId, CoordinateSpace, DesktopGeneration, DesktopId, EffectStage,
    ElementClickScrollPolicy, ElementOcclusionPolicy, ElementPhysicalClickCommand,
    ElementPhysicalClickResult, ElementScrollAlignment, ElementScrollCommand, ElementScrollTarget,
    ElementSnapshotExpansion, ElementSnapshotRequest, ElementWindowActivationPolicy, ErrorCode,
    EventResyncReason, EventTopic, LeaseAcquireRequest, LeaseAvailability, LeaseReleaseRequest,
    LeaseRenewRequest, LeaseStateView, MAX_CLIPBOARD_PRESERVATION_BYTES,
    MAX_PASTE_OBSERVATION_TIMEOUT_MS, MAX_SELECTION_BYTES, MAX_TEXT_INSERT_BYTES, NormalizedEvent,
    PROCESS_EXITED_TOPIC, Point, PointerClickTarget, PointerCurve, PointerDragTarget,
    PointerLogicalButton, PointerScrollDirection, Problem, ProcessExitedEvent, Rect, RetryAdvice,
    SelectionName, SelectionSetCommand, SelectionTransferEvidence, SelectionTransferTerminal,
    SequencedEvent, Sha256Digest, TextInsertCommand, TextInsertEvidence, TextInsertOptions,
    TextSource, TextStrategy, TextTarget, Timestamp, TracePolicy, WindowActivateCommand,
    WindowActivateResult, WindowCloseOutcome, WindowCloseResult, WindowControlResult,
    WindowControlWarning, WindowFocusFallback, WindowGeometryRequest, WindowGeometryTarget,
    WindowManagerState, WindowMoveResizeResult, WindowMoveToWorkspaceResult,
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
    accessibility_plane::{
        AccessibilityCorrelationCoordinator, AccessibilityCorrelationCoordinatorConfig,
        AccessibilityCorrelationCoordinatorError, AccessibilityExplicitCorrelationEvidence,
        AccessibilityProfiledRect, accessibility_correlation_error_class,
    },
    accessibility_runtime::AccessibilitySemanticRuntime,
    artifact_service::{InternalArtifactContext, StoreArtifactService},
    observation_plane::{DaemonObservationService, WindowOcclusionSnapshot},
    semantic_actions::{
        SemanticActionFailure, execute_semantic_action, execute_semantic_text_insert,
        require_supported_postcondition, wait_for_postcondition,
    },
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
const PROCESS_CORRELATION_INVALIDATION_TIMEOUT: Duration = Duration::from_millis(250);
const RESYNC_BARRIER_RETENTION_CHARGE: usize = 64;
const EXTERNAL_EVENT_QUEUE_CAPACITY: usize = 1_024;

type RuntimeHandle = CoordinatorHandle<RuntimeCommand, RuntimeResult, RuntimeEvent>;
type ProcessEventSourceFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ProcessEventSubscription, ProcessEventSourceError>> + Send + 'a>,
>;
type ProcessLiveEventFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BrokerLiveEvent, ProcessEventSourceError>> + Send + 'a>>;

trait ProcessEventSource: Send + Sync {
    fn subscribe<'a>(
        &'a self,
        generation: DesktopGeneration,
        cursor: u64,
    ) -> ProcessEventSourceFuture<'a>;
}

trait ProcessLiveEventSource: Send {
    fn receive<'a>(&'a mut self) -> ProcessLiveEventFuture<'a>;
}

struct ProcessEventSubscription {
    replay: BrokerEventReplay,
    live: Box<dyn ProcessLiveEventSource>,
}

struct BrokerProcessEventSource(BrokerClient);

impl ProcessEventSource for BrokerProcessEventSource {
    fn subscribe<'a>(
        &'a self,
        generation: DesktopGeneration,
        cursor: u64,
    ) -> ProcessEventSourceFuture<'a> {
        Box::pin(async move {
            let xenoteer_processd::BrokerEventSubscription { replay, live } =
                self.0.subscribe_events(generation, cursor).await?;
            Ok(ProcessEventSubscription {
                replay,
                live: Box::new(BrokerProcessLiveEventSource(live)),
            })
        })
    }
}

struct BrokerProcessLiveEventSource(BrokerEventStream);

impl ProcessLiveEventSource for BrokerProcessLiveEventSource {
    fn receive<'a>(&'a mut self) -> ProcessLiveEventFuture<'a> {
        Box::pin(async move { self.0.receive().await.map_err(Into::into) })
    }
}

#[derive(Debug, Error)]
enum ProcessEventSourceError {
    #[error(transparent)]
    Broker(#[from] BrokerClientError),
    #[cfg(test)]
    #[error("injected process event source failure")]
    Injected,
}

/// Owned coordinator task and its HTTP adapter.
pub(crate) struct CoordinatorRuntime {
    handle: RuntimeHandle,
    join: JoinHandle<()>,
    process_event_cancellation: CancellationToken,
    process_event_join: JoinHandle<Result<(), ProcessEventRelayError>>,
    accessibility_correlation_join: Option<JoinHandle<()>>,
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
        if let Some(join) = self.accessibility_correlation_join {
            join.await
                .map_err(|_| CoordinatorRuntimeError::AccessibilityCorrelationTaskPanicked)?;
        }
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
    spawn_inner(config, desktop_id, generation, input, None, None, None)
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
        None,
    )
}

/// Creates the coordinator with the complete Phase-4 clipboard/text runtime.
#[allow(dead_code)]
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
        None,
    )
}

/// Creates the coordinator with clipboard and generation-fenced semantic actions.
pub(crate) fn spawn_with_accessibility_runtime(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    clipboard: ClipboardRuntime,
    accessibility: AccessibilitySemanticRuntime,
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
        Some(accessibility),
    )
}

fn spawn_inner(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    input: watch::Receiver<Option<InputActorHandle>>,
    window_control: Option<WindowControlRuntime>,
    clipboard: Option<ClipboardRuntime>,
    accessibility: Option<AccessibilitySemanticRuntime>,
) -> Result<CoordinatorRuntime, CoordinatorSetupError> {
    let limits = config.limits();
    let root_bounds = Rect::new(
        0,
        0,
        config.desktop().display_width(),
        config.desktop().display_height(),
    )?;
    let accessibility_correlation = match (&accessibility, &window_control) {
        (Some(accessibility), Some(window_control)) => {
            Some(Arc::new(AccessibilityCorrelationCoordinator::live(
                accessibility.plane(),
                accessibility.handle(),
                Arc::clone(&window_control.observation),
                AccessibilityCorrelationCoordinatorConfig::default(),
            )?))
        }
        _ => None,
    };
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
    let process_correlation_invalidator = window_control.as_ref().map(|window_control| {
        Arc::clone(&window_control.observation) as Arc<dyn ProcessCorrelationInvalidator>
    });
    let correlation_runtime = accessibility_correlation.clone();
    let executor = RuntimeExecutor {
        input,
        window_control,
        clipboard,
        accessibility,
        accessibility_correlation,
        broker: broker.clone(),
        motion_policy: MotionPolicy::from_input_config(config.input())?,
        desktop_id,
        generation,
        root_bounds,
    };
    let event_mapper = RuntimeEventMapper::new()?;
    let (handle, join) = spawn_coordinator_with_event_mapper(settings, executor, event_mapper)?;
    let process_event_cancellation = CancellationToken::new();
    let accessibility_correlation_join =
        correlation_runtime.map(|runtime| runtime.spawn(process_event_cancellation.child_token()));
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
            process_correlation_invalidator,
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
        accessibility_correlation_join,
        external_event_ingress,
        external_event_join,
        control,
    })
}

/// Cloneable daemon composition over the dedicated raw window-control actor.
#[derive(Clone)]
pub(crate) struct WindowControlRuntime {
    actor: Arc<dyn WindowControlBackend>,
    observation: Arc<DaemonObservationService>,
}

pub(crate) type WindowControlRevalidator =
    Box<dyn FnOnce() -> Result<(), RawWindowRevalidationError> + Send + 'static>;

trait WindowControlBackend: Send + Sync {
    fn execute(
        &self,
        request: RawWindowControlRequest,
        revalidate: WindowControlRevalidator,
        timeout: Duration,
    ) -> Result<RawWindowControlEvidence, WindowControlBackendError>;
}

struct LiveWindowControlBackend {
    actor: WindowControlActorHandle,
}

pub(crate) enum WindowControlBackendError {
    Submit(WindowControlSubmitError),
    Actor(WindowControlActorFailureKind),
    ReplyUnavailable,
}

impl WindowControlBackend for LiveWindowControlBackend {
    fn execute(
        &self,
        request: RawWindowControlRequest,
        revalidate: WindowControlRevalidator,
        timeout: Duration,
    ) -> Result<RawWindowControlEvidence, WindowControlBackendError> {
        let reply = self
            .actor
            .try_submit(request, revalidate)
            .map_err(WindowControlBackendError::Submit)?;
        match reply.recv_timeout(timeout) {
            Ok(Ok(evidence)) => Ok(evidence),
            Ok(Err(failure)) => Err(WindowControlBackendError::Actor(failure.kind)),
            Err(_) => Err(WindowControlBackendError::ReplyUnavailable),
        }
    }
}

#[cfg(test)]
pub(crate) type ScriptedWindowControlHandler = dyn Fn(
        RawWindowControlRequest,
        WindowControlRevalidator,
        Duration,
    ) -> Result<RawWindowControlEvidence, WindowControlBackendError>
    + Send
    + Sync;

#[cfg(test)]
struct ScriptedWindowControlBackend {
    handler: Arc<ScriptedWindowControlHandler>,
}

#[cfg(test)]
impl WindowControlBackend for ScriptedWindowControlBackend {
    fn execute(
        &self,
        request: RawWindowControlRequest,
        revalidate: WindowControlRevalidator,
        timeout: Duration,
    ) -> Result<RawWindowControlEvidence, WindowControlBackendError> {
        (self.handler)(request, revalidate, timeout)
    }
}

#[derive(Clone)]
struct WindowMutationFence {
    stop_requested: Option<Arc<AtomicBool>>,
    cancellation: Option<CancellationToken>,
    deadline: Option<Instant>,
}

impl WindowMutationFence {
    fn requested(stop_requested: Arc<AtomicBool>) -> Self {
        Self {
            stop_requested: Some(stop_requested),
            cancellation: None,
            deadline: None,
        }
    }

    fn physical(cancellation: CancellationToken, deadline: Instant) -> Self {
        Self {
            stop_requested: None,
            cancellation: Some(cancellation),
            deadline: Some(deadline),
        }
    }

    fn is_stopped(&self) -> bool {
        self.stop_requested
            .as_ref()
            .is_some_and(|requested| requested.load(Ordering::Acquire))
            || self
                .cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

fn revalidate_at_window_effect_boundary<F>(
    fence: &WindowMutationFence,
    revalidate: F,
) -> Result<(), RawWindowRevalidationError>
where
    F: FnOnce() -> Result<(), RawWindowRevalidationError>,
{
    if fence.is_stopped() {
        return Err(RawWindowRevalidationError::Rejected);
    }
    revalidate()?;
    // Window-model revalidation can block. Recheck after it so cancellation
    // or deadline expiry during that read cannot authorize a later X11 effect.
    if fence.is_stopped() {
        return Err(RawWindowRevalidationError::Rejected);
    }
    Ok(())
}

impl WindowControlRuntime {
    pub(crate) fn new(
        actor: WindowControlActorHandle,
        observation: Arc<DaemonObservationService>,
    ) -> Self {
        Self {
            actor: Arc::new(LiveWindowControlBackend { actor }),
            observation,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_scripted(
        observation: Arc<DaemonObservationService>,
        handler: Arc<ScriptedWindowControlHandler>,
    ) -> Self {
        Self {
            actor: Arc::new(ScriptedWindowControlBackend { handler }),
            observation,
        }
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
    accessibility: Option<AccessibilitySemanticRuntime>,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
}

impl ClipboardRuntime {
    /// Binds every authority-bearing dependency used by clipboard/text commands.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: ClipboardActorHandle,
        artifacts: Arc<StoreArtifactService>,
        observation: Arc<DaemonObservationService>,
        window_control: WindowControlRuntime,
        input: watch::Receiver<Option<InputActorHandle>>,
        accessibility: Option<AccessibilitySemanticRuntime>,
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Self {
        Self {
            actor,
            artifacts,
            observation,
            window_control,
            input,
            accessibility,
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

    fn semantic_runtime(&self) -> Option<AccessibilitySemanticRuntime> {
        None
    }

    fn semantic_text_insert<'a>(
        &'a self,
        element: xenoteer_protocol::ElementRef,
        text: String,
        options: xenoteer_protocol::SemanticTextInsertOptions,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Option<
        ClipboardRuntimeFuture<
            'a,
            Result<xenoteer_protocol::SemanticTextInsertEvidence, SemanticActionFailure>,
        >,
    > {
        let runtime = self.semantic_runtime()?;
        Some(Box::pin(async move {
            execute_semantic_text_insert(&runtime, element, text, options, deadline, cancellation)
                .await
        }))
    }

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

    fn semantic_runtime(&self) -> Option<AccessibilitySemanticRuntime> {
        self.accessibility.clone()
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
    detailed_trace: bool,
    command: Command,
}

#[derive(Clone, Debug, PartialEq)]
struct RuntimeSuccess {
    outcome: CommandOutcome,
    effect_stage: EffectStage,
    trace: Option<CommandTrace>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeFailure {
    status: u16,
    code: ErrorCode,
    title: &'static str,
    detail: &'static str,
    retry: RetryAdvice,
    effect_stage: EffectStage,
    trace_progress: RuntimeTraceProgress,
    trace: Option<CommandTrace>,
}

#[derive(Clone, Debug, PartialEq)]
enum RuntimeResult {
    Success(Box<RuntimeSuccess>),
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
    process_correlations: Option<Arc<dyn ProcessCorrelationInvalidator>>,
) -> Result<(), ProcessEventRelayError> {
    let source = BrokerProcessEventSource(broker);
    let final_authority = process_correlations.clone();
    let result = relay_process_events_inner(
        ingress,
        &source,
        desktop_generation,
        cancellation,
        process_correlations,
    )
    .await;
    if let Some(authority) = final_authority.as_deref() {
        authority.disable();
    }
    match result {
        Err(ProcessEventRelayError::Cancelled) => Ok(()),
        result => result,
    }
}

async fn relay_process_events_inner(
    ingress: ExternalEventIngress,
    source: &dyn ProcessEventSource,
    desktop_generation: DesktopGeneration,
    cancellation: CancellationToken,
    process_correlations: Option<Arc<dyn ProcessCorrelationInvalidator>>,
) -> Result<(), ProcessEventRelayError> {
    let authority =
        ProcessCorrelationAuthorityGuard::new(process_correlations.as_deref(), &cancellation);
    let mut cursor = 0_u64;
    let mut reconnect_delay = PROCESS_EVENT_RECONNECT_INITIAL;

    loop {
        let subscription = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = source.subscribe(desktop_generation, cursor) => result,
        };
        let subscription = match subscription {
            Ok(subscription) => subscription,
            Err(error) => {
                invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                    .await?;
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

        let ProcessEventSubscription { replay, mut live } = subscription;
        let mut replay_ambiguous = false;
        match replay {
            BrokerEventReplay::Events {
                latest_sequence,
                events,
            } => {
                for event in events {
                    let outcome = relay_process_event(
                        &ingress,
                        desktop_generation,
                        &cancellation,
                        cursor,
                        event,
                        process_correlations.as_deref(),
                    )
                    .await?;
                    cursor = outcome.cursor();
                    replay_ambiguous |=
                        matches!(outcome, ProcessRelayEventOutcome::Ambiguous { .. });
                }
                if cursor != latest_sequence {
                    invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                        .await?;
                    require_process_resync(&ingress, &cancellation)?;
                    cursor = latest_sequence;
                    replay_ambiguous = true;
                }
            }
            BrokerEventReplay::ResyncRequired {
                latest_sequence, ..
            } => {
                invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                    .await?;
                require_process_resync(&ingress, &cancellation)?;
                cursor = latest_sequence;
            }
        }
        if replay_ambiguous {
            if wait_for_process_event_reconnect(&cancellation, reconnect_delay).await {
                return Ok(());
            }
            reconnect_delay = reconnect_delay
                .checked_mul(2)
                .unwrap_or(PROCESS_EVENT_RECONNECT_MAXIMUM)
                .min(PROCESS_EVENT_RECONNECT_MAXIMUM);
            continue;
        }
        authority.enable_if_live()?;

        let reconnect_after_failure = loop {
            let item = tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = live.receive() => result,
            };
            match item {
                Ok(BrokerLiveEvent::Event(event)) => {
                    let outcome = relay_process_event(
                        &ingress,
                        desktop_generation,
                        &cancellation,
                        cursor,
                        event,
                        process_correlations.as_deref(),
                    )
                    .await?;
                    cursor = outcome.cursor();
                    match outcome {
                        ProcessRelayEventOutcome::Applied { .. } => {
                            reconnect_delay = PROCESS_EVENT_RECONNECT_INITIAL;
                            authority.enable_if_live()?;
                        }
                        ProcessRelayEventOutcome::Ambiguous { .. } => break true,
                    }
                }
                Ok(BrokerLiveEvent::ResyncRequired {
                    latest_sequence, ..
                }) => {
                    invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                        .await?;
                    require_process_resync(&ingress, &cancellation)?;
                    cursor = latest_sequence;
                    break true;
                }
                Ok(BrokerLiveEvent::Closed) => {
                    invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                        .await?;
                    break true;
                }
                Err(error) => {
                    invalidate_process_correlations(process_correlations.as_deref(), &cancellation)
                        .await?;
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
    process_correlations: Option<&dyn ProcessCorrelationInvalidator>,
) -> Result<ProcessRelayEventOutcome, ProcessEventRelayError> {
    let sequence = event.sequence();
    if sequence <= cursor {
        return Ok(ProcessRelayEventOutcome::Applied { cursor });
    }
    invalidate_process_correlations(process_correlations, cancellation).await?;
    let sequence_gap = sequence != cursor.saturating_add(1);
    if sequence_gap {
        require_process_resync(ingress, cancellation)?;
    }
    let invalid = match normalize_process_event(desktop_generation, event) {
        Ok(event) => {
            publish_process_event(ingress, cancellation, event)?;
            false
        }
        Err(error) => {
            tracing::error!(error = %error, "invalid process event rejected");
            require_process_resync(ingress, cancellation)?;
            true
        }
    };
    if sequence_gap || invalid {
        Ok(ProcessRelayEventOutcome::Ambiguous { cursor: sequence })
    } else {
        Ok(ProcessRelayEventOutcome::Applied { cursor: sequence })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessRelayEventOutcome {
    Applied { cursor: u64 },
    Ambiguous { cursor: u64 },
}

impl ProcessRelayEventOutcome {
    fn cursor(self) -> u64 {
        match self {
            Self::Applied { cursor } | Self::Ambiguous { cursor } => cursor,
        }
    }
}

trait ProcessCorrelationInvalidator: Send + Sync {
    fn disable(&self);

    fn enable(&self);

    fn invalidate<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ControlPlaneError>> + Send + 'a>>;
}

struct ProcessCorrelationAuthorityGuard<'a> {
    invalidator: Option<&'a dyn ProcessCorrelationInvalidator>,
    cancellation: &'a CancellationToken,
}

impl<'a> ProcessCorrelationAuthorityGuard<'a> {
    fn new(
        invalidator: Option<&'a dyn ProcessCorrelationInvalidator>,
        cancellation: &'a CancellationToken,
    ) -> Self {
        if let Some(invalidator) = invalidator {
            invalidator.disable();
        }
        Self {
            invalidator,
            cancellation,
        }
    }

    fn enable_if_live(&self) -> Result<(), ProcessEventRelayError> {
        if self.cancellation.is_cancelled() {
            return Err(ProcessEventRelayError::Cancelled);
        }
        if let Some(invalidator) = self.invalidator {
            invalidator.enable();
            if self.cancellation.is_cancelled() {
                invalidator.disable();
                return Err(ProcessEventRelayError::Cancelled);
            }
        }
        Ok(())
    }
}

impl Drop for ProcessCorrelationAuthorityGuard<'_> {
    fn drop(&mut self) {
        if let Some(invalidator) = self.invalidator {
            invalidator.disable();
        }
    }
}

impl ProcessCorrelationInvalidator for DaemonObservationService {
    fn disable(&self) {
        self.disable_process_lifecycle_authority();
    }

    fn enable(&self) {
        self.enable_process_lifecycle_authority();
    }

    fn invalidate<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ControlPlaneError>> + Send + 'a>> {
        Box::pin(self.invalidate_process_correlations())
    }
}

async fn invalidate_process_correlations(
    invalidator: Option<&dyn ProcessCorrelationInvalidator>,
    cancellation: &CancellationToken,
) -> Result<(), ProcessEventRelayError> {
    let Some(invalidator) = invalidator else {
        return Ok(());
    };
    invalidator.disable();
    loop {
        let result = tokio::select! {
            () = cancellation.cancelled() => return Err(ProcessEventRelayError::Cancelled),
            result = tokio::time::timeout(
                PROCESS_CORRELATION_INVALIDATION_TIMEOUT,
                invalidator.invalidate(),
            ) => result,
        };
        if matches!(result, Ok(Ok(()))) {
            return Ok(());
        }
        tracing::warn!("process correlation invalidation unavailable; retrying while fenced");
        if wait_for_process_event_reconnect(cancellation, PROCESS_EVENT_RECONNECT_INITIAL).await {
            return Err(ProcessEventRelayError::Cancelled);
        }
    }
}

#[cfg(test)]
pub(crate) async fn relay_process_event_for_observation_test(
    observation: Arc<DaemonObservationService>,
    desktop_generation: DesktopGeneration,
    event: BrokerProcessEvent,
) -> Result<(), ProcessEventRelayError> {
    let (sender, _receiver) = mpsc::channel(4);
    let ingress = ExternalEventIngress {
        sender,
        resync_state: Arc::new(AtomicU64::new(0)),
        resync_notify: Arc::new(Notify::new()),
    };
    let outcome = relay_process_event(
        &ingress,
        desktop_generation,
        &CancellationToken::new(),
        0,
        event,
        Some(observation.as_ref()),
    )
    .await?;
    if matches!(outcome, ProcessRelayEventOutcome::Applied { .. }) {
        observation.enable_process_lifecycle_authority();
    }
    Ok(())
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
        Self::Success(Box::new(RuntimeSuccess {
            outcome,
            effect_stage,
            trace: None,
        }))
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
            trace_progress: RuntimeTraceProgress::None,
            trace: None,
        })
    }

    const fn effect_stage(&self) -> EffectStage {
        match self {
            Self::Success(result) => result.effect_stage,
            Self::Failure(result) => result.effect_stage,
        }
    }

    fn attach_trace(&mut self, trace: CommandTrace) {
        match self {
            Self::Success(result) => result.trace = Some(trace),
            Self::Failure(result) => result.trace = Some(trace),
        }
    }

    fn with_trace_progress(mut self, progress: RuntimeTraceProgress) -> Self {
        if let Self::Failure(failure) = &mut self {
            failure.trace_progress = progress;
        }
        self
    }

    const fn trace(&self) -> Option<&CommandTrace> {
        match self {
            Self::Success(result) => result.trace.as_ref(),
            Self::Failure(result) => result.trace.as_ref(),
        }
    }

    fn preserve_prior_effect(mut self, prior: EffectStage) -> Self {
        if prior.has_visible_effect() {
            let retain_prior_stage = !self.effect_stage().has_visible_effect();
            match &mut self {
                Self::Success(result) if retain_prior_stage => result.effect_stage = prior,
                Self::Failure(result) => {
                    if retain_prior_stage {
                        result.effect_stage = prior;
                    }
                    result.retry = RetryAdvice::Never;
                }
                Self::Success(_) => {}
            }
        }
        self
    }

    fn forbid_retry_after_effect(mut self) -> Self {
        if self.effect_stage().has_visible_effect() {
            match &mut self {
                Self::Success(_) => {}
                Self::Failure(failure) => failure.retry = RetryAdvice::Never,
            }
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum RuntimeTraceProgress {
    #[default]
    None,
    SemanticDispatched,
    SemanticReadback,
}

#[derive(Clone, Copy)]
enum RuntimeTraceIntent {
    General,
    Semantic,
    TextInsert,
    PhysicalElement,
}

impl RuntimeTraceIntent {
    const fn from_command(command: &Command) -> Self {
        match command {
            Command::ElementInvoke(_)
            | Command::ElementFocus(_)
            | Command::ElementSetValue(_)
            | Command::ElementSelection(_)
            | Command::ElementSetText(_)
            | Command::ElementInsertText(_)
            | Command::ElementScroll(_) => Self::Semantic,
            Command::ElementPhysicalClick(_) => Self::PhysicalElement,
            Command::TextInsert(_) => Self::TextInsert,
            _ => Self::General,
        }
    }
}

fn attach_detailed_trace(
    outcome: ExecutionOutcome<RuntimeResult>,
    intent: RuntimeTraceIntent,
) -> ExecutionOutcome<RuntimeResult> {
    match outcome {
        ExecutionOutcome::Completed { mut output, effect } => {
            attach_runtime_trace(&mut output, intent);
            ExecutionOutcome::Completed { output, effect }
        }
        ExecutionOutcome::AtomicCompleted { mut output, effect } => {
            attach_runtime_trace(&mut output, intent);
            ExecutionOutcome::AtomicCompleted { output, effect }
        }
        ExecutionOutcome::Stopped { effect } => {
            let stage = if effect == CommandEffect::AfterEffect {
                EffectStage::SideEffectObserved
            } else {
                EffectStage::None
            };
            let progress = if effect == CommandEffect::AfterEffect
                && matches!(intent, RuntimeTraceIntent::Semantic)
            {
                RuntimeTraceProgress::SemanticDispatched
            } else {
                RuntimeTraceProgress::None
            };
            let mut output = RuntimeResult::failure(
                500,
                ErrorCode::Internal,
                "Command stopped",
                "The coordinator stopped command execution.",
                RetryAdvice::Never,
                stage,
            )
            .with_trace_progress(progress);
            attach_runtime_trace(&mut output, intent);
            ExecutionOutcome::StoppedWithEvidence { output, effect }
        }
        ExecutionOutcome::StoppedWithEvidence { mut output, effect } => {
            attach_runtime_trace(&mut output, intent);
            ExecutionOutcome::StoppedWithEvidence { output, effect }
        }
    }
}

fn attach_runtime_trace(result: &mut RuntimeResult, intent: RuntimeTraceIntent) {
    let trace = detailed_runtime_trace(result, intent);
    match trace {
        Ok(trace) => result.attach_trace(trace),
        Err(_) => {
            let stage = if result.effect_stage().has_visible_effect() {
                EffectStage::OutcomeUnknown
            } else {
                EffectStage::None
            };
            *result = RuntimeResult::failure(
                500,
                ErrorCode::Internal,
                "Command trace invariant failed",
                "The command completed, but its bounded trace evidence was invalid.",
                RetryAdvice::Never,
                stage,
            );
        }
    }
}

fn detailed_runtime_trace(
    result: &RuntimeResult,
    intent: RuntimeTraceIntent,
) -> Result<CommandTrace, xenoteer_protocol::CommandTraceValidationError> {
    let succeeded = matches!(result, RuntimeResult::Success(_));
    let terminal_status = if succeeded {
        CommandTraceStatus::Completed
    } else {
        CommandTraceStatus::Failed
    };
    let intent = match (intent, result) {
        (RuntimeTraceIntent::TextInsert, RuntimeResult::Success(success))
            if matches!(
                &success.outcome,
                CommandOutcome::TextInserted { evidence }
                    if evidence.selected_strategy == TextStrategy::Semantic
            ) =>
        {
            RuntimeTraceIntent::Semantic
        }
        (RuntimeTraceIntent::TextInsert, _) => RuntimeTraceIntent::General,
        (intent, _) => intent,
    };
    let completed = CommandTraceStep {
        stage: CommandTraceStage::CommandCompleted,
        status: terminal_status,
    };
    let mut steps = vec![CommandTraceStep {
        stage: CommandTraceStage::CommandValidated,
        status: CommandTraceStatus::Completed,
    }];
    let domain = match intent {
        RuntimeTraceIntent::General | RuntimeTraceIntent::TextInsert => CommandTraceDomain::General,
        RuntimeTraceIntent::Semantic => {
            let progress = match result {
                RuntimeResult::Success(_) => RuntimeTraceProgress::SemanticReadback,
                RuntimeResult::Failure(failure) => failure.trace_progress,
            };
            if progress >= RuntimeTraceProgress::SemanticDispatched {
                steps.extend([
                    CommandTraceStep {
                        stage: CommandTraceStage::SemanticTargetRevalidated,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::SemanticDispatched,
                        status: CommandTraceStatus::Completed,
                    },
                ]);
            }
            if progress >= RuntimeTraceProgress::SemanticReadback {
                steps.extend([CommandTraceStep {
                    stage: CommandTraceStage::SemanticReadback,
                    status: CommandTraceStatus::Completed,
                }]);
            }
            if succeeded {
                steps.extend([CommandTraceStep {
                    stage: CommandTraceStage::PostconditionObserved,
                    status: if result_has_postcondition(result) {
                        CommandTraceStatus::Completed
                    } else {
                        CommandTraceStatus::NotRequired
                    },
                }]);
            }
            CommandTraceDomain::SemanticAccessibility
        }
        RuntimeTraceIntent::PhysicalElement => {
            if let RuntimeResult::Success(success) = result
                && let CommandOutcome::ElementPhysicalClick { result } = &success.outcome
            {
                steps.extend([
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalCorrelationRevalidated,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalWindowRevalidated,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalScroll,
                        status: if result.scrolled {
                            CommandTraceStatus::Completed
                        } else {
                            CommandTraceStatus::NotRequired
                        },
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalActivation,
                        status: if result.window_activated {
                            CommandTraceStatus::Completed
                        } else {
                            CommandTraceStatus::NotRequired
                        },
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalPointerInterpolation,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalButtonPress,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PhysicalButtonRelease,
                        status: CommandTraceStatus::Completed,
                    },
                    CommandTraceStep {
                        stage: CommandTraceStage::PostconditionObserved,
                        status: if result.postcondition_satisfied == Some(true) {
                            CommandTraceStatus::Completed
                        } else {
                            CommandTraceStatus::NotRequired
                        },
                    },
                ]);
            }
            CommandTraceDomain::PhysicalElementInput
        }
    };
    steps.push(completed);
    CommandTrace::new(domain, steps)
}

fn result_has_postcondition(result: &RuntimeResult) -> bool {
    match result {
        RuntimeResult::Success(success) => match &success.outcome {
            CommandOutcome::ElementAction { result } => {
                result.evidence.postcondition_satisfied == Some(true)
            }
            CommandOutcome::TextInserted { evidence } => evidence
                .semantic
                .as_ref()
                .is_some_and(|semantic| semantic.postcondition_satisfied == Some(true)),
            _ => false,
        },
        RuntimeResult::Failure(_) => false,
    }
}

#[derive(Clone)]
struct RuntimeExecutor {
    input: watch::Receiver<Option<InputActorHandle>>,
    window_control: Option<WindowControlRuntime>,
    clipboard: Option<ClipboardRuntime>,
    accessibility: Option<AccessibilitySemanticRuntime>,
    accessibility_correlation: Option<Arc<AccessibilityCorrelationCoordinator>>,
    broker: BrokerClient,
    motion_policy: MotionPolicy,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    root_bounds: Rect,
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
        let detailed_trace = command.detailed_trace;
        let trace_intent = RuntimeTraceIntent::from_command(&command.command);
        let outcome = self.execute_command_untraced(command, context).await;
        if detailed_trace {
            attach_detailed_trace(outcome, trace_intent)
        } else {
            outcome
        }
    }

    async fn execute_command_untraced(
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
            command @ (Command::ElementInvoke(_)
            | Command::ElementFocus(_)
            | Command::ElementSetValue(_)
            | Command::ElementSelection(_)
            | Command::ElementSetText(_)
            | Command::ElementInsertText(_)
            | Command::ElementScroll(_)) => {
                let Some(runtime) = self.accessibility.clone() else {
                    return completed(capability_unavailable());
                };
                execute_semantic_command(&runtime, command, context).await
            }
            Command::ElementPhysicalClick(command) => {
                execute_physical_element_click(self, command_id, command, context).await
            }
        }
    }
}

async fn execute_semantic_command(
    runtime: &AccessibilitySemanticRuntime,
    command: Command,
    mut context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    if !matches!(context.stop_reason(), ExecutionStop::Continue) {
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        };
    }
    let deadline = context
        .deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let mut operation = Box::pin(execute_semantic_action(
        runtime,
        command,
        deadline,
        cancellation.clone(),
    ));
    tokio::select! {
        result = &mut operation => completed(match result {
            Ok(result) => {
                let stage = semantic_success_stage(
                    result.operation,
                    result.evidence.postcondition_satisfied == Some(true),
                );
                RuntimeResult::success(CommandOutcome::ElementAction { result }, stage)
            }
            Err(error) => map_semantic_failure(error),
        }),
        _reason = context.wait_for_stop() => {
            cancellation.cancel();
            let effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut operation).await {
                Ok(Ok(_)) => CommandEffect::AfterEffect,
                Ok(Err(error)) if error.effect_may_have_occurred() => CommandEffect::AfterEffect,
                Ok(Err(_)) => CommandEffect::BeforeEffect,
                Err(_) => CommandEffect::AfterEffect,
            };
            ExecutionOutcome::Stopped { effect }
        }
    }
}

fn semantic_success_stage(
    operation: xenoteer_protocol::ElementActionOperation,
    postcondition_satisfied: bool,
) -> EffectStage {
    if postcondition_satisfied {
        return EffectStage::PostconditionMet;
    }
    match operation {
        xenoteer_protocol::ElementActionOperation::Invoke
        | xenoteer_protocol::ElementActionOperation::Scroll => {
            EffectStage::SemanticActionDispatched
        }
        xenoteer_protocol::ElementActionOperation::InsertText => EffectStage::TextInserted,
        xenoteer_protocol::ElementActionOperation::Focus
        | xenoteer_protocol::ElementActionOperation::SetValue
        | xenoteer_protocol::ElementActionOperation::Selection
        | xenoteer_protocol::ElementActionOperation::SetText => EffectStage::SemanticStateChanged,
    }
}

fn map_semantic_failure(error: SemanticActionFailure) -> RuntimeResult {
    let trace_progress = semantic_failure_trace_progress(&error);
    let after_effect = error.effect_may_have_occurred();
    let stage = if after_effect {
        EffectStage::SemanticActionDispatched
    } else {
        EffectStage::None
    };
    let result = match error {
        SemanticActionFailure::PlaneBefore(error) => map_semantic_plane_error(error, false),
        SemanticActionFailure::PlaneAfter(error) => map_semantic_plane_error(error, true),
        SemanticActionFailure::Actor(error) => match error {
            xenoteer_atspi::SemanticError::QueueFull => resource_exhausted(),
            xenoteer_atspi::SemanticError::StaleAccessibilityGeneration { .. }
            | xenoteer_atspi::SemanticError::StaleApplicationGeneration { .. }
            | xenoteer_atspi::SemanticError::StaleCacheRevision { .. }
            | xenoteer_atspi::SemanticError::StaleIdentity => stale_element_reference(),
            xenoteer_atspi::SemanticError::InterfaceUnavailable(_) => RuntimeResult::failure(
                422,
                ErrorCode::InterfaceNotSupported,
                "Element interface not supported",
                "The exact element does not expose the interface required by this operation.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            xenoteer_atspi::SemanticError::UnclassifiedTextDenied => {
                semantic_verification_unsupported()
            }
            xenoteer_atspi::SemanticError::ActionNotFound => RuntimeResult::failure(
                404,
                ErrorCode::ActionNotFound,
                "Semantic action not found",
                "The requested semantic action name, index, or default is absent.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            xenoteer_atspi::SemanticError::AmbiguousAction => RuntimeResult::failure(
                409,
                ErrorCode::AmbiguousTarget,
                "Semantic action is ambiguous",
                "The requested semantic action did not resolve to one unique action.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            xenoteer_atspi::SemanticError::InvalidRequest(_) => invalid_input(),
            xenoteer_atspi::SemanticError::CancelledBeforeDispatch => RuntimeResult::failure(
                409,
                ErrorCode::CancelledBeforeEffect,
                "Command cancelled before effect",
                "Cancellation was observed before the semantic method was dispatched.",
                RetryAdvice::SameCommandId,
                EffectStage::None,
            ),
            xenoteer_atspi::SemanticError::DeadlineBeforeDispatch => RuntimeResult::failure(
                504,
                ErrorCode::DeadlineExceededBeforeEffect,
                "Command deadline exceeded before effect",
                "The deadline elapsed before the semantic method was dispatched.",
                RetryAdvice::SameCommandId,
                EffectStage::None,
            ),
            xenoteer_atspi::SemanticError::CancelledAfterDispatch => RuntimeResult::failure(
                409,
                ErrorCode::CancelledAfterEffect,
                "Command cancelled after effect",
                "Cancellation raced with a dispatched semantic method.",
                RetryAdvice::Never,
                stage,
            ),
            xenoteer_atspi::SemanticError::DeadlineAfterDispatch => RuntimeResult::failure(
                504,
                ErrorCode::DeadlineExceededAfterEffect,
                "Command deadline exceeded after effect",
                "The deadline elapsed after the semantic method was dispatched.",
                RetryAdvice::Never,
                stage,
            ),
            xenoteer_atspi::SemanticError::ReplyLostAfterAdmission
            | xenoteer_atspi::SemanticError::BackendAfterDispatch(_) => RuntimeResult::failure(
                502,
                ErrorCode::RequestOutcomeUnknown,
                "Semantic command outcome unknown",
                "The semantic method may have been dispatched, but completion could not be proven.",
                RetryAdvice::SameCommandId,
                EffectStage::OutcomeUnknown,
            ),
            xenoteer_atspi::SemanticError::Stopped | xenoteer_atspi::SemanticError::Unavailable => {
                capability_unavailable()
            }
            xenoteer_atspi::SemanticError::Backend(_)
            | xenoteer_atspi::SemanticError::ReadEpochExhausted => backend_failure(stage),
        },
        SemanticActionFailure::Disabled => RuntimeResult::failure(
            409,
            ErrorCode::ElementNotSensitive,
            "Element is not sensitive",
            "The element is not enabled and sensitive for this semantic action.",
            RetryAdvice::AfterResync,
            EffectStage::None,
        ),
        SemanticActionFailure::WeakWindowCorrelation => RuntimeResult::failure(
            409,
            ErrorCode::WeakWindowCorrelation,
            "Window correlation required",
            "The element is not correlated to a current X11 window.",
            RetryAdvice::AfterResync,
            EffectStage::None,
        ),
        SemanticActionFailure::VerificationUnsupported => semantic_verification_unsupported(),
        SemanticActionFailure::BackendRejected => RuntimeResult::failure(
            422,
            ErrorCode::UnsupportedByTarget,
            "Semantic action rejected",
            "The target rejected the semantic operation after it was dispatched.",
            RetryAdvice::Never,
            stage,
        ),
        SemanticActionFailure::PostconditionFailed => semantic_postcondition_failed(),
        SemanticActionFailure::DeadlineAfterEffect => RuntimeResult::failure(
            504,
            ErrorCode::DeadlineExceededAfterEffect,
            "Command deadline exceeded after effect",
            "The semantic method completed, but its required observation exceeded the deadline.",
            RetryAdvice::Never,
            stage,
        ),
        SemanticActionFailure::InvalidEvidence => RuntimeResult::failure(
            502,
            ErrorCode::RequestOutcomeUnknown,
            "Semantic command outcome unknown",
            "The backend returned contradictory semantic completion evidence.",
            RetryAdvice::Never,
            EffectStage::OutcomeUnknown,
        ),
    };
    result.with_trace_progress(trace_progress)
}

fn semantic_failure_trace_progress(error: &SemanticActionFailure) -> RuntimeTraceProgress {
    match error {
        SemanticActionFailure::PlaneAfter(_)
        | SemanticActionFailure::PostconditionFailed
        | SemanticActionFailure::DeadlineAfterEffect => RuntimeTraceProgress::SemanticReadback,
        SemanticActionFailure::BackendRejected => RuntimeTraceProgress::SemanticDispatched,
        SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::CancelledAfterDispatch
            | xenoteer_atspi::SemanticError::DeadlineAfterDispatch
            | xenoteer_atspi::SemanticError::ReplyLostAfterAdmission
            | xenoteer_atspi::SemanticError::BackendAfterDispatch(_),
        ) => RuntimeTraceProgress::SemanticDispatched,
        _ => RuntimeTraceProgress::None,
    }
}

fn map_semantic_plane_error(
    error: xenoteer_server::AccessibilityPlaneError,
    after: bool,
) -> RuntimeResult {
    if after {
        return match error {
            xenoteer_server::AccessibilityPlaneError::ResourceExhausted => resource_exhausted(),
            _ => semantic_postcondition_failed(),
        };
    }
    match error {
        xenoteer_server::AccessibilityPlaneError::InvalidRequest => invalid_input(),
        xenoteer_server::AccessibilityPlaneError::PermissionDenied => permission_denied(),
        xenoteer_server::AccessibilityPlaneError::NotFound => RuntimeResult::failure(
            404,
            ErrorCode::ElementNotFound,
            "Element not found",
            "The exact accessibility element does not exist.",
            RetryAdvice::AfterResync,
            EffectStage::None,
        ),
        xenoteer_server::AccessibilityPlaneError::StaleReference { .. }
        | xenoteer_server::AccessibilityPlaneError::ResyncRequired { .. } => {
            stale_element_reference()
        }
        xenoteer_server::AccessibilityPlaneError::AmbiguousTarget => RuntimeResult::failure(
            409,
            ErrorCode::AmbiguousTarget,
            "Ambiguous accessibility target",
            "The semantic target did not resolve to one exact element.",
            RetryAdvice::AfterResync,
            EffectStage::None,
        ),
        xenoteer_server::AccessibilityPlaneError::QueryLimitExceeded => RuntimeResult::failure(
            422,
            ErrorCode::QueryBudgetExceeded,
            "Accessibility query budget exceeded",
            "The semantic precondition exceeded its bounded query budget.",
            RetryAdvice::Never,
            EffectStage::None,
        ),
        xenoteer_server::AccessibilityPlaneError::ResourceExhausted => resource_exhausted(),
        xenoteer_server::AccessibilityPlaneError::CapabilityUnavailable => capability_unavailable(),
        xenoteer_server::AccessibilityPlaneError::UnsupportedByTarget => {
            semantic_verification_unsupported()
        }
        xenoteer_server::AccessibilityPlaneError::Internal => backend_failure(EffectStage::None),
    }
}

fn stale_element_reference() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::StaleReference,
        "Accessibility element changed",
        "The exact generation-fenced element reference is no longer current.",
        RetryAdvice::AfterResync,
        EffectStage::None,
    )
}

fn semantic_verification_unsupported() -> RuntimeResult {
    RuntimeResult::failure(
        422,
        ErrorCode::UnsupportedByTarget,
        "Semantic verification unsupported",
        "The target cannot provide the content-free verification required by this operation.",
        RetryAdvice::Never,
        EffectStage::None,
    )
}

fn semantic_postcondition_failed() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::SemanticPostconditionFailed,
        "Semantic postcondition failed",
        "The semantic method was dispatched, but the required readback was not observed.",
        RetryAdvice::Never,
        EffectStage::SemanticActionDispatched,
    )
}

struct PreparedPhysicalClick {
    correlation: AccessibilityExplicitCorrelationEvidence,
    plan: PhysicalElementClickPlan,
}

#[derive(Debug)]
enum PhysicalClickPrepareFailure {
    Correlation(AccessibilityCorrelationCoordinatorError),
    Plan(ElementClickPlanError),
    Window(ControlPlaneError),
    Geometry,
    BlockingTask,
}

#[derive(Clone, Copy, Debug)]
enum PhysicalClickPreconditionIssue {
    Stale,
    WeakCorrelation,
    Geometry,
    Occluded,
    FocusLost,
    ResourceExhausted,
    Unavailable,
}

impl PhysicalClickPreconditionIssue {
    const fn input_failure(self) -> InputPreconditionFailure {
        match self {
            Self::Stale | Self::WeakCorrelation | Self::Geometry | Self::Occluded => {
                InputPreconditionFailure::TargetStale
            }
            Self::FocusLost => InputPreconditionFailure::FocusLost,
            Self::ResourceExhausted | Self::Unavailable => InputPreconditionFailure::Unavailable,
        }
    }
}

fn profile_accessibility_rect(
    bounds: SemanticRect,
) -> Result<AccessibilityProfiledRect, PhysicalClickPrepareFailure> {
    let width = u32::try_from(bounds.width).map_err(|_| PhysicalClickPrepareFailure::Geometry)?;
    let height = u32::try_from(bounds.height).map_err(|_| PhysicalClickPrepareFailure::Geometry)?;
    let root_physical = Rect::new(bounds.x, bounds.y, width, height)
        .map_err(|_| PhysicalClickPrepareFailure::Geometry)?;
    // Release-one owns a fixed single-screen Xvfb profile. This deliberately
    // binds one exact raw AT-SPI screen rectangle to its root-pixel projection;
    // it is not a generic claim that arbitrary AT-SPI coordinates are root pixels.
    Ok(AccessibilityProfiledRect {
        atspi_screen: bounds,
        root_physical,
    })
}

fn profile_optional_accessibility_rect(
    bounds: Option<SemanticRect>,
) -> Result<Option<AccessibilityProfiledRect>, PhysicalClickPrepareFailure> {
    bounds.map(profile_accessibility_rect).transpose()
}

fn physical_click_timeout(deadline: Instant) -> Result<Duration, PhysicalClickPrepareFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(PhysicalClickPrepareFailure::Window(
            ControlPlaneError::CapabilityUnavailable,
        ));
    }
    Ok(remaining.min(WINDOW_REVALIDATION_TIMEOUT))
}

async fn admission_occlusion_snapshot(
    observation: Arc<DaemonObservationService>,
    window: WindowRef,
    policy: ElementOcclusionPolicy,
    deadline: Instant,
) -> Result<Option<WindowOcclusionSnapshot>, PhysicalClickPrepareFailure> {
    if policy == ElementOcclusionPolicy::Ignore {
        return Ok(None);
    }
    let timeout = physical_click_timeout(deadline)?;
    tokio::task::spawn_blocking(move || {
        observation.occlusion_snapshot_exact_blocking(window, timeout)
    })
    .await
    .map_err(|_| PhysicalClickPrepareFailure::BlockingTask)?
    .map(Some)
    .map_err(PhysicalClickPrepareFailure::Window)
}

async fn prepare_physical_click(
    coordinator: &AccessibilityCorrelationCoordinator,
    observation: Arc<DaemonObservationService>,
    command: &ElementPhysicalClickCommand,
    root_bounds: Rect,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<PreparedPhysicalClick, PhysicalClickPrepareFailure> {
    let correlation = coordinator
        .correlate_element(
            &command.element,
            command.window.clone(),
            deadline,
            cancellation,
        )
        .await
        .map_err(PhysicalClickPrepareFailure::Correlation)?;
    let element_extents = correlation
        .admission_element_observation()
        .evidence
        .bounds
        .ok_or(PhysicalClickPrepareFailure::Geometry)
        .and_then(profile_accessibility_rect)?;
    let top_level_extents = profile_optional_accessibility_rect(
        correlation
            .admission_correlation_observation()
            .evidence
            .bounds,
    )?;
    let click_observation = correlation
        .click_observation(
            element_extents,
            top_level_extents,
            root_bounds,
            AccessibilityCorrelationLimits::default(),
        )
        .map_err(PhysicalClickPrepareFailure::Correlation)?;
    let correlated_window =
        click_observation
            .correlation
            .window
            .clone()
            .ok_or(PhysicalClickPrepareFailure::Plan(
                ElementClickPlanError::UnauthorizedCorrelation,
            ))?;
    let occlusion = admission_occlusion_snapshot(
        observation,
        correlated_window,
        command.occlusion_policy,
        deadline,
    )
    .await?;
    let plan = plan_physical_element_click(
        &click_observation,
        command.window.as_ref(),
        command.minimum_correlation,
        &command.point_policy,
        command.occlusion_policy,
        occlusion
            .as_ref()
            .map(WindowOcclusionSnapshot::as_click_snapshot),
    )
    .map_err(PhysicalClickPrepareFailure::Plan)?;
    Ok(PreparedPhysicalClick { correlation, plan })
}

fn map_physical_prepare_failure(
    failure: PhysicalClickPrepareFailure,
    prior_stage: EffectStage,
) -> RuntimeResult {
    tracing::debug!(
        error = ?failure,
        effect_stage = ?prior_stage,
        "physical element admission failed closed"
    );
    let result = match failure {
        PhysicalClickPrepareFailure::Correlation(error) => map_physical_correlation_failure(error),
        PhysicalClickPrepareFailure::Plan(error) => map_physical_plan_failure(error),
        PhysicalClickPrepareFailure::Window(error) => {
            map_window_model_error(error, EffectStage::None)
        }
        PhysicalClickPrepareFailure::Geometry => physical_geometry_invalid(),
        PhysicalClickPrepareFailure::BlockingTask => backend_failure(EffectStage::None),
    };
    result.preserve_prior_effect(prior_stage)
}

fn map_physical_correlation_failure(
    error: AccessibilityCorrelationCoordinatorError,
) -> RuntimeResult {
    match error {
        AccessibilityCorrelationCoordinatorError::Cancelled => RuntimeResult::failure(
            409,
            ErrorCode::CancelledBeforeEffect,
            "Physical click cancelled before effect",
            "Cancellation was observed while resolving element-to-window evidence.",
            RetryAdvice::SameCommandId,
            EffectStage::None,
        ),
        AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted => {
            stale_element_reference()
        }
        AccessibilityCorrelationCoordinatorError::Plane(error) => {
            map_semantic_plane_error(error, false)
        }
        AccessibilityCorrelationCoordinatorError::Window(error) => {
            map_window_model_error(error, EffectStage::None)
        }
        AccessibilityCorrelationCoordinatorError::Actor(error) => match error {
            xenoteer_atspi::SemanticError::QueueFull => resource_exhausted(),
            xenoteer_atspi::SemanticError::StaleAccessibilityGeneration { .. }
            | xenoteer_atspi::SemanticError::StaleApplicationGeneration { .. }
            | xenoteer_atspi::SemanticError::StaleCacheRevision { .. }
            | xenoteer_atspi::SemanticError::StaleIdentity => stale_element_reference(),
            xenoteer_atspi::SemanticError::Stopped | xenoteer_atspi::SemanticError::Unavailable => {
                capability_unavailable()
            }
            _ => backend_failure(EffectStage::None),
        },
        AccessibilityCorrelationCoordinatorError::Correlation(error) => match error {
            xenoteer_core::AccessibilityCorrelationError::CandidateLimit
            | xenoteer_core::AccessibilityCorrelationError::StringLimit => RuntimeResult::failure(
                422,
                ErrorCode::QueryBudgetExceeded,
                "Correlation budget exceeded",
                "Fresh element-to-window correlation exceeded its bounded evidence budget.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            xenoteer_core::AccessibilityCorrelationError::InvalidGeometry => {
                physical_geometry_invalid()
            }
            _ => backend_failure(EffectStage::None),
        },
        AccessibilityCorrelationCoordinatorError::InvalidConfiguration => capability_unavailable(),
    }
}

fn map_physical_plan_failure(error: ElementClickPlanError) -> RuntimeResult {
    match error {
        ElementClickPlanError::UnauthorizedCorrelation
        | ElementClickPlanError::InvalidMinimumCorrelation
        | ElementClickPlanError::CorrelationBelowMinimum => weak_physical_correlation(),
        ElementClickPlanError::Occluded | ElementClickPlanError::OcclusionInconclusive => {
            RuntimeResult::failure(
                409,
                ErrorCode::ElementOccluded,
                "Element click point is occluded",
                "The requested occlusion policy could not prove the selected click point clear.",
                RetryAdvice::AfterResync,
                EffectStage::None,
            )
        }
        ElementClickPlanError::ElementBirthChanged
        | ElementClickPlanError::RevisionRegression
        | ElementClickPlanError::WindowBindingChanged
        | ElementClickPlanError::ObservationNotFresh
        | ElementClickPlanError::OcclusionTargetChanged
        | ElementClickPlanError::OcclusionSnapshotNotFresh => stale_element_reference(),
        ElementClickPlanError::InvalidElementReference
        | ElementClickPlanError::InvalidWindowReference
        | ElementClickPlanError::ReferenceScope
        | ElementClickPlanError::InvalidGeometry
        | ElementClickPlanError::NotVisible
        | ElementClickPlanError::InsetExhausted
        | ElementClickPlanError::PointOverflow
        | ElementClickPlanError::PointOutsideVisibleBounds
        | ElementClickPlanError::InvalidReadEpoch
        | ElementClickPlanError::GeometryChanged
        | ElementClickPlanError::InvalidOcclusionEpoch
        | ElementClickPlanError::MissingQueueHeadOcclusionSnapshot => physical_geometry_invalid(),
    }
}

fn weak_physical_correlation() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::WeakWindowCorrelation,
        "Element-to-window correlation is insufficient",
        "Fresh evidence could not authorize this element-derived physical input effect.",
        RetryAdvice::AfterResync,
        EffectStage::None,
    )
}

fn physical_geometry_invalid() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::ElementGeometryInvalid,
        "Element click geometry is invalid",
        "Fresh element geometry could not be bound to one unchanged root-physical click point.",
        RetryAdvice::AfterResync,
        EffectStage::None,
    )
}

async fn execute_physical_element_click(
    executor: &RuntimeExecutor,
    command_id: CommandId,
    command: ElementPhysicalClickCommand,
    mut context: ExecutionContext,
) -> ExecutionOutcome<RuntimeResult> {
    if !matches!(context.stop_reason(), ExecutionStop::Continue) {
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        };
    }
    let deadline = context
        .deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let mut operation = Box::pin(execute_physical_element_click_inner(
        executor,
        command_id,
        command,
        deadline,
        cancellation.clone(),
    ));
    tokio::select! {
        result = &mut operation => completed(result),
        _reason = context.wait_for_stop() => {
            cancellation.cancel();
            let effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut operation).await {
                Ok(result) if result.effect_stage().has_visible_effect() => CommandEffect::AfterEffect,
                Ok(_) => CommandEffect::BeforeEffect,
                Err(_) => CommandEffect::AfterEffect,
            };
            ExecutionOutcome::Stopped { effect }
        }
    }
}

async fn execute_physical_element_click_inner(
    executor: &RuntimeExecutor,
    command_id: CommandId,
    command: ElementPhysicalClickCommand,
    deadline: Instant,
    cancellation: CancellationToken,
) -> RuntimeResult {
    if command.validate().is_err() {
        return invalid_input();
    }
    if let Err(error) = require_supported_postcondition(command.postcondition.as_ref()) {
        return map_semantic_failure(error);
    }
    let Some(runtime) = executor.accessibility.as_ref() else {
        return capability_unavailable();
    };
    let Some(correlation) = executor.accessibility_correlation.as_ref() else {
        return capability_unavailable();
    };
    let Some(window_control) = executor.window_control.as_ref() else {
        return capability_unavailable();
    };
    let Some(input) = executor.input.borrow().clone() else {
        return capability_unavailable();
    };

    let mut prior_stage = EffectStage::None;
    let mut scrolled = false;
    if command.scroll_policy == ElementClickScrollPolicy::Always {
        match perform_physical_click_scroll(runtime, &command, deadline, cancellation.child_token())
            .await
        {
            Ok(stage) => {
                prior_stage = stage;
                scrolled = true;
            }
            Err(result) => return result,
        }
    }

    let mut prepared = loop {
        let prepared = prepare_physical_click(
            correlation,
            Arc::clone(&window_control.observation),
            &command,
            executor.root_bounds,
            deadline,
            cancellation.child_token(),
        )
        .await;
        match prepared {
            Ok(prepared) => break prepared,
            Err(PhysicalClickPrepareFailure::Plan(ElementClickPlanError::NotVisible))
                if command.scroll_policy == ElementClickScrollPolicy::IfNeeded && !scrolled =>
            {
                match perform_physical_click_scroll(
                    runtime,
                    &command,
                    deadline,
                    cancellation.child_token(),
                )
                .await
                {
                    Ok(stage) => {
                        prior_stage = stage;
                        scrolled = true;
                    }
                    Err(result) => return result.preserve_prior_effect(prior_stage),
                }
            }
            Err(error) => return map_physical_prepare_failure(error, prior_stage),
        }
    };

    let (window_activated, activation_stage) = match activate_physical_click_window(
        window_control.clone(),
        prepared.plan.window().clone(),
        command.activation_policy,
        prior_stage,
        deadline,
        cancellation.child_token(),
    )
    .await
    {
        Ok(value) => value,
        Err(result) => return result.preserve_prior_effect(prior_stage),
    };
    if activation_stage.has_visible_effect() {
        prior_stage = activation_stage;
    }
    if window_activated {
        let admitted_window = prepared.plan.window().clone();
        prepared = match prepare_physical_click(
            correlation,
            Arc::clone(&window_control.observation),
            &command,
            executor.root_bounds,
            deadline,
            cancellation.child_token(),
        )
        .await
        {
            Ok(refreshed) if refreshed.plan.window() == &admitted_window => refreshed,
            Ok(_) => return stale_element_reference().preserve_prior_effect(prior_stage),
            Err(error) => return map_physical_prepare_failure(error, prior_stage),
        };
    }
    if cancellation.is_cancelled() {
        return cancelled_physical_click(prior_stage);
    }

    let options = match input_motion_options(
        command.curve,
        command.move_duration_ms,
        executor.motion_policy,
    ) {
        Ok(options) if options.curve() != MotionCurve::Instant => options,
        _ => return invalid_input().preserve_prior_effect(prior_stage),
    };
    let click_point = prepared.plan.click_point();
    let expected_root = match RootPoint::try_from_protocol(click_point) {
        Ok(point) => point,
        Err(_) => return physical_geometry_invalid().preserve_prior_effect(prior_stage),
    };
    let local_point = match client_local_click_point(&prepared.plan, click_point) {
        Ok(point) => point,
        Err(result) => return result.preserve_prior_effect(prior_stage),
    };
    let interval_ms = match u16::try_from(command.interval_ms) {
        Ok(value) => value,
        Err(_) => return invalid_input().preserve_prior_effect(prior_stage),
    };
    let request = WindowPointerClickRequest::new(
        prepared.plan.window().xid,
        CoordinateSpace::WindowClient,
        local_point,
        X11WindowPointerBoundsPolicy::Reject,
        options,
        input_logical_button(command.button),
        command.count,
        0,
        0,
        interval_ms,
    )
    .with_expected_root_target(expected_root);
    let (precondition, latest_queue, precondition_issue) = physical_click_precondition(
        runtime.clone(),
        Arc::clone(&window_control.observation),
        prepared.correlation.clone(),
        prepared.plan.clone(),
        executor.root_bounds,
        command.occlusion_policy,
        command.activation_policy != ElementWindowActivationPolicy::Never,
        deadline,
    );
    let receiver = match input.try_submit_operation_with_precondition(
        ActionContext::new(command_id, Some(deadline.into_std())),
        InputOperation::WindowPointerClick(request),
        precondition,
        cancellation,
    ) {
        Ok(receiver) => receiver,
        Err(InputSubmitError::QueueFull) => {
            return resource_exhausted().preserve_prior_effect(prior_stage);
        }
        Err(InputSubmitError::Closed) => {
            return capability_unavailable().preserve_prior_effect(prior_stage);
        }
    };
    let input_result = match receiver.await {
        Ok(result) => result,
        Err(_) => {
            return backend_failure(EffectStage::OutcomeUnknown).preserve_prior_effect(prior_stage);
        }
    };
    finish_physical_click(
        runtime,
        command,
        prepared.plan,
        latest_queue,
        precondition_issue,
        input_result,
        scrolled,
        window_activated,
        prior_stage,
        deadline,
    )
    .await
}

async fn perform_physical_click_scroll(
    runtime: &AccessibilitySemanticRuntime,
    command: &ElementPhysicalClickCommand,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<EffectStage, RuntimeResult> {
    let scroll = Command::ElementScroll(ElementScrollCommand {
        element: command.element.clone(),
        target: ElementScrollTarget::Alignment {
            alignment: ElementScrollAlignment::Anywhere,
        },
        postcondition: None,
    });
    execute_semantic_action(runtime, scroll, deadline, cancellation)
        .await
        .map(|_| EffectStage::SemanticActionDispatched)
        .map_err(map_semantic_failure)
}

async fn activate_physical_click_window(
    runtime: WindowControlRuntime,
    window: WindowRef,
    policy: ElementWindowActivationPolicy,
    prior_stage: EffectStage,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<(bool, EffectStage), RuntimeResult> {
    if policy == ElementWindowActivationPolicy::Never {
        return Ok((false, EffectStage::None));
    }
    if cancellation.is_cancelled() {
        return Err(cancelled_physical_click(prior_stage));
    }
    if Instant::now() >= deadline {
        return Err(deadline_exceeded_physical_click(prior_stage));
    }

    let execution_cancellation = cancellation.clone();
    let mut activation = tokio::task::spawn_blocking(move || {
        let snapshot = runtime
            .observation
            .snapshot_exact_blocking(window.clone(), WINDOW_REVALIDATION_TIMEOUT)
            .map_err(|error| map_window_model_error(error, EffectStage::None))?;
        if snapshot.state.focused {
            return Ok((false, EffectStage::None));
        }
        let result = runtime.execute_fenced(
            Command::WindowActivate(WindowActivateCommand {
                window,
                switch_workspace: true,
                fallback: WindowFocusFallback::EwmhOnly,
            }),
            WindowMutationFence::physical(execution_cancellation, deadline),
        );
        match result {
            RuntimeResult::Success(success) => Ok((true, success.effect_stage)),
            RuntimeResult::Failure(_) => Err(result),
        }
    });

    tokio::select! {
        biased;
        result = &mut activation => {
            finish_physical_window_activation(result, &cancellation, prior_stage, deadline)
        }
        () = cancellation.cancelled() => {
            match tokio::time::timeout(WINDOW_REVALIDATION_TIMEOUT, &mut activation).await {
                Ok(result) => finish_physical_window_activation(
                    result,
                    &cancellation,
                    prior_stage,
                    deadline,
                ),
                Err(_) => Err(backend_failure(EffectStage::OutcomeUnknown)),
            }
        }
        () = tokio::time::sleep_until(deadline) => {
            cancellation.cancel();
            match tokio::time::timeout(WINDOW_REVALIDATION_TIMEOUT, &mut activation).await {
                Ok(result) => finish_physical_window_activation(
                    result,
                    &cancellation,
                    prior_stage,
                    deadline,
                ),
                Err(_) => Err(backend_failure(EffectStage::OutcomeUnknown)),
            }
        }
    }
}

fn finish_physical_window_activation(
    result: Result<Result<(bool, EffectStage), RuntimeResult>, tokio::task::JoinError>,
    cancellation: &CancellationToken,
    prior_stage: EffectStage,
    deadline: Instant,
) -> Result<(bool, EffectStage), RuntimeResult> {
    let result = result.unwrap_or_else(|_| Err(backend_failure(EffectStage::OutcomeUnknown)));
    let effect_stage = match &result {
        Ok((_, effect_stage)) => *effect_stage,
        Err(result) => result.effect_stage(),
    };
    if effect_stage.has_visible_effect() || effect_stage == EffectStage::OutcomeUnknown {
        return result;
    }
    if Instant::now() >= deadline {
        return Err(deadline_exceeded_physical_click(prior_stage));
    }
    if cancellation.is_cancelled() {
        return Err(cancelled_physical_click(prior_stage));
    }
    result
}

fn client_local_click_point(
    plan: &PhysicalElementClickPlan,
    root: Point,
) -> Result<Point, RuntimeResult> {
    let client = plan
        .geometry()
        .correlated_client_bounds
        .ok_or_else(physical_geometry_invalid)?;
    let origin = client.origin();
    let x = root
        .x()
        .checked_sub(origin.x())
        .ok_or_else(physical_geometry_invalid)?;
    let y = root
        .y()
        .checked_sub(origin.y())
        .ok_or_else(physical_geometry_invalid)?;
    Ok(Point::new(x, y))
}

fn cancelled_physical_click(prior_stage: EffectStage) -> RuntimeResult {
    let (code, title, detail, retry) = if prior_stage.has_visible_effect() {
        (
            ErrorCode::CancelledAfterEffect,
            "Physical click cancelled after effect",
            "Cancellation was observed after semantic scroll or window activation.",
            RetryAdvice::Never,
        )
    } else {
        (
            ErrorCode::CancelledBeforeEffect,
            "Physical click cancelled before effect",
            "Cancellation was observed before physical input changed the desktop.",
            RetryAdvice::SameCommandId,
        )
    };
    RuntimeResult::failure(409, code, title, detail, retry, prior_stage)
}

fn deadline_exceeded_physical_click(prior_stage: EffectStage) -> RuntimeResult {
    let (code, title, detail, retry) = if prior_stage.has_visible_effect() {
        (
            ErrorCode::DeadlineExceededAfterEffect,
            "Physical click deadline exceeded after effect",
            "The deadline elapsed after semantic scroll or window activation.",
            RetryAdvice::Never,
        )
    } else {
        (
            ErrorCode::DeadlineExceededBeforeEffect,
            "Physical click deadline exceeded before effect",
            "The deadline elapsed before physical input changed the desktop.",
            RetryAdvice::SameCommandId,
        )
    };
    RuntimeResult::failure(504, code, title, detail, retry, prior_stage)
}

type SharedQueueClickEvidence = Arc<Mutex<Option<RevalidatedElementClick>>>;
type SharedPhysicalClickIssue = Arc<Mutex<Option<PhysicalClickPreconditionIssue>>>;

#[allow(clippy::too_many_arguments)]
fn physical_click_precondition(
    runtime: AccessibilitySemanticRuntime,
    observation: Arc<DaemonObservationService>,
    correlation: AccessibilityExplicitCorrelationEvidence,
    plan: PhysicalElementClickPlan,
    root_bounds: Rect,
    occlusion_policy: ElementOcclusionPolicy,
    require_focus: bool,
    deadline: Instant,
) -> (
    InputPrecondition,
    SharedQueueClickEvidence,
    SharedPhysicalClickIssue,
) {
    let latest = Arc::new(Mutex::new(None));
    let issue = Arc::new(Mutex::new(None));
    let latest_for_check = Arc::clone(&latest);
    let issue_for_check = Arc::clone(&issue);
    let precondition = InputPrecondition::new(move || {
        match run_physical_click_precondition(
            &runtime,
            &observation,
            &correlation,
            &plan,
            root_bounds,
            occlusion_policy,
            require_focus,
            deadline,
        ) {
            Ok(evidence) => {
                *latest_for_check
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(evidence);
                *issue_for_check
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                Ok(())
            }
            Err(failure) => {
                *issue_for_check
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failure);
                Err(failure.input_failure())
            }
        }
    });
    (precondition, latest, issue)
}

#[allow(clippy::too_many_arguments)]
fn run_physical_click_precondition(
    runtime: &AccessibilitySemanticRuntime,
    observation: &DaemonObservationService,
    correlation: &AccessibilityExplicitCorrelationEvidence,
    plan: &PhysicalElementClickPlan,
    root_bounds: Rect,
    occlusion_policy: ElementOcclusionPolicy,
    require_focus: bool,
    deadline: Instant,
) -> Result<RevalidatedElementClick, PhysicalClickPreconditionIssue> {
    let queue_deadline = deadline.min(Instant::now() + WINDOW_REVALIDATION_TIMEOUT);
    let plane = runtime.plane();
    let actor = runtime.handle();
    let fresh = correlation
        .refresh_for_queue_head_blocking(
            plane.as_ref(),
            &actor,
            observation,
            queue_deadline,
            AccessibilityCorrelationLimits::default(),
        )
        .map_err(|error| {
            tracing::debug!(
                error_class = accessibility_correlation_error_class(&error),
                "physical element queue-head correlation failed closed"
            );
            map_physical_precondition_correlation_error(error)
        })?;
    let element_extents = fresh
        .admission_element_observation()
        .evidence
        .bounds
        .ok_or(PhysicalClickPreconditionIssue::Geometry)
        .and_then(|bounds| {
            profile_accessibility_rect(bounds).map_err(|_| PhysicalClickPreconditionIssue::Geometry)
        })?;
    let top_level_extents = fresh
        .admission_correlation_observation()
        .evidence
        .bounds
        .map(profile_accessibility_rect)
        .transpose()
        .map_err(|_| PhysicalClickPreconditionIssue::Geometry)?;
    let click = fresh
        .click_observation(
            element_extents,
            top_level_extents,
            root_bounds,
            AccessibilityCorrelationLimits::default(),
        )
        .map_err(map_physical_precondition_correlation_error)?;
    if require_focus {
        let focused = observation
            .snapshot_exact_blocking(
                plan.window().clone(),
                remaining_physical_precondition_timeout(deadline)?,
            )
            .map_err(map_physical_precondition_window_error)?;
        if !focused.state.focused {
            return Err(PhysicalClickPreconditionIssue::FocusLost);
        }
    }
    let occlusion = if occlusion_policy == ElementOcclusionPolicy::Ignore {
        None
    } else {
        Some(
            observation
                .occlusion_snapshot_exact_blocking(
                    plan.window().clone(),
                    remaining_physical_precondition_timeout(deadline)?,
                )
                .map_err(map_physical_precondition_window_error)?,
        )
    };
    revalidate_physical_element_click(
        plan,
        &click,
        occlusion
            .as_ref()
            .map(WindowOcclusionSnapshot::as_click_snapshot),
    )
    .map_err(map_physical_precondition_plan_error)
}

fn remaining_physical_precondition_timeout(
    deadline: Instant,
) -> Result<Duration, PhysicalClickPreconditionIssue> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(PhysicalClickPreconditionIssue::Unavailable)
    } else {
        Ok(remaining.min(WINDOW_REVALIDATION_TIMEOUT))
    }
}

fn map_physical_precondition_window_error(
    error: ControlPlaneError,
) -> PhysicalClickPreconditionIssue {
    match error {
        ControlPlaneError::NotFound
        | ControlPlaneError::StaleReference { .. }
        | ControlPlaneError::PermissionDenied => PhysicalClickPreconditionIssue::Stale,
        ControlPlaneError::ResourceExhausted => PhysicalClickPreconditionIssue::ResourceExhausted,
        _ => PhysicalClickPreconditionIssue::Unavailable,
    }
}

fn map_physical_precondition_correlation_error(
    error: AccessibilityCorrelationCoordinatorError,
) -> PhysicalClickPreconditionIssue {
    match error {
        AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted
        | AccessibilityCorrelationCoordinatorError::Plane(
            xenoteer_server::AccessibilityPlaneError::NotFound
            | xenoteer_server::AccessibilityPlaneError::StaleReference { .. }
            | xenoteer_server::AccessibilityPlaneError::ResyncRequired { .. },
        ) => PhysicalClickPreconditionIssue::Stale,
        AccessibilityCorrelationCoordinatorError::Plane(
            xenoteer_server::AccessibilityPlaneError::ResourceExhausted,
        )
        | AccessibilityCorrelationCoordinatorError::Actor(
            xenoteer_atspi::SemanticError::QueueFull,
        )
        | AccessibilityCorrelationCoordinatorError::Window(ControlPlaneError::ResourceExhausted) => {
            PhysicalClickPreconditionIssue::ResourceExhausted
        }
        AccessibilityCorrelationCoordinatorError::Correlation(
            xenoteer_core::AccessibilityCorrelationError::InvalidGeometry,
        ) => PhysicalClickPreconditionIssue::Geometry,
        _ => PhysicalClickPreconditionIssue::Unavailable,
    }
}

fn map_physical_precondition_plan_error(
    error: ElementClickPlanError,
) -> PhysicalClickPreconditionIssue {
    match error {
        ElementClickPlanError::UnauthorizedCorrelation
        | ElementClickPlanError::InvalidMinimumCorrelation
        | ElementClickPlanError::CorrelationBelowMinimum => {
            PhysicalClickPreconditionIssue::WeakCorrelation
        }
        ElementClickPlanError::Occluded | ElementClickPlanError::OcclusionInconclusive => {
            PhysicalClickPreconditionIssue::Occluded
        }
        ElementClickPlanError::ElementBirthChanged
        | ElementClickPlanError::RevisionRegression
        | ElementClickPlanError::WindowBindingChanged
        | ElementClickPlanError::ObservationNotFresh
        | ElementClickPlanError::OcclusionTargetChanged
        | ElementClickPlanError::OcclusionSnapshotNotFresh => PhysicalClickPreconditionIssue::Stale,
        _ => PhysicalClickPreconditionIssue::Geometry,
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_physical_click(
    runtime: &AccessibilitySemanticRuntime,
    command: ElementPhysicalClickCommand,
    plan: PhysicalElementClickPlan,
    latest_queue: SharedQueueClickEvidence,
    precondition_issue: SharedPhysicalClickIssue,
    input_result: Result<InputOutcome, InputFailure>,
    scrolled: bool,
    window_activated: bool,
    prior_stage: EffectStage,
    deadline: Instant,
) -> RuntimeResult {
    let outcome = match input_result {
        Ok(outcome) => outcome,
        Err(failure) => {
            let issue = *precondition_issue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return map_physical_input_failure(failure, issue, prior_stage);
        }
    };
    let stage = precise_input_effect_stage(
        outcome.events_emitted,
        outcome.completed_units,
        true,
        Some(&outcome.effects),
        InputStage::PointerClick,
        prior_stage,
    );
    match outcome.kind {
        InputOutcomeKind::CancelledAfterEffect => {
            return RuntimeResult::failure(
                409,
                ErrorCode::CancelledAfterEffect,
                "Physical click cancelled after effect",
                "Cancellation was observed after element-derived physical input began.",
                RetryAdvice::Never,
                stage,
            );
        }
        InputOutcomeKind::DeadlineExceededAfterEffect => {
            return RuntimeResult::failure(
                504,
                ErrorCode::DeadlineExceededAfterEffect,
                "Physical click deadline exceeded after effect",
                "The deadline elapsed after element-derived physical input began.",
                RetryAdvice::Never,
                stage,
            );
        }
        InputOutcomeKind::Completed => {}
    }
    let queue = *latest_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(queue) = queue else {
        return backend_failure(EffectStage::OutcomeUnknown).preserve_prior_effect(stage);
    };
    let expected_root = RootPoint::try_from_protocol(plan.click_point()).ok();
    if outcome.completed_units != u16::from(command.count)
        || outcome.events_emitted == 0
        || outcome.requested_pointer != expected_root
        || outcome.observed_pointer != expected_root
    {
        return backend_failure(EffectStage::OutcomeUnknown).preserve_prior_effect(stage);
    }

    let mut postcondition_satisfied = None;
    let mut final_snapshot = None;
    if let Some(postcondition) = &command.postcondition {
        let mut bounded = postcondition.clone();
        bounded.timeout_ms = bounded.timeout_ms.min(command.settle_timeout_ms);
        match wait_for_postcondition(
            &runtime.plane(),
            &command.element,
            queue.revision_after_queue,
            &bounded,
            deadline,
        )
        .await
        {
            Ok(wait) => {
                postcondition_satisfied = Some(true);
                final_snapshot = wait
                    .elements
                    .into_iter()
                    .find(|entry| entry.snapshot.element == command.element)
                    .map(|entry| Box::new(entry.snapshot));
            }
            Err(_) => return physical_postcondition_failed(),
        }
    } else if let Ok(snapshot) = runtime
        .plane()
        .snapshot_for(ElementSnapshotRequest {
            desktop_id: command.element.desktop_id,
            desktop_generation: command.element.desktop_generation,
            element: command.element.clone(),
            expansion: ElementSnapshotExpansion::default(),
        })
        .await
    {
        final_snapshot = Some(Box::new(snapshot.element.snapshot));
    }

    let result = ElementPhysicalClickResult {
        element: command.element,
        window: plan.window().clone(),
        correlation: queue.correlation,
        revision_before_queue: plan.revision_before_queue(),
        revision_after_queue: queue.revision_after_queue,
        extents_before_queue: plan.geometry().element_extents,
        extents_after_queue: queue.extents_after_queue,
        click_point: plan.click_point(),
        occlusion_check: queue.occlusion_check,
        scrolled,
        window_activated,
        pointer_interpolated: true,
        button: command.button,
        count: command.count,
        postcondition_satisfied,
        final_snapshot,
    };
    if result.validate().is_err() {
        return backend_failure(EffectStage::OutcomeUnknown);
    }
    RuntimeResult::success(
        CommandOutcome::ElementPhysicalClick { result },
        if postcondition_satisfied == Some(true) {
            EffectStage::PostconditionMet
        } else {
            EffectStage::ElementPhysicallyClicked
        },
    )
}

fn map_physical_input_failure(
    failure: InputFailure,
    issue: Option<PhysicalClickPreconditionIssue>,
    prior_stage: EffectStage,
) -> RuntimeResult {
    tracing::debug!(
        error = ?failure,
        precondition_issue = ?issue,
        effect_stage = ?prior_stage,
        "physical element input failed closed"
    );
    let effect_stage = precise_input_effect_stage(
        failure.events_emitted,
        failure.completed_units,
        failure.progress_known,
        failure.effects.as_deref(),
        InputStage::PointerClick,
        prior_stage,
    );
    if let Some(issue) = issue {
        let result = match issue {
            PhysicalClickPreconditionIssue::Stale => stale_element_reference(),
            PhysicalClickPreconditionIssue::WeakCorrelation => weak_physical_correlation(),
            PhysicalClickPreconditionIssue::Geometry => physical_geometry_invalid(),
            PhysicalClickPreconditionIssue::Occluded => RuntimeResult::failure(
                409,
                ErrorCode::ElementOccluded,
                "Element click point became occluded",
                "Fresh queue-head stacking evidence rejected the selected click point.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            PhysicalClickPreconditionIssue::FocusLost => RuntimeResult::failure(
                409,
                ErrorCode::WeakWindowCorrelation,
                "Target window focus was lost",
                "The activated exact target window was not focused at the input boundary.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            PhysicalClickPreconditionIssue::ResourceExhausted => resource_exhausted(),
            PhysicalClickPreconditionIssue::Unavailable => backend_failure(EffectStage::None),
        };
        return result
            .preserve_prior_effect(effect_stage)
            .forbid_retry_after_effect();
    }
    if failure.kind == InputFailureKind::PostconditionFailed {
        return physical_geometry_invalid()
            .preserve_prior_effect(effect_stage)
            .forbid_retry_after_effect();
    }
    input_failure(failure, InputStage::PointerClick, prior_stage).forbid_retry_after_effect()
}

fn physical_postcondition_failed() -> RuntimeResult {
    RuntimeResult::failure(
        409,
        ErrorCode::SemanticPostconditionFailed,
        "Physical click postcondition failed",
        "The click completed, but its requested semantic postcondition was not observed.",
        RetryAdvice::Never,
        EffectStage::ElementPhysicallyClicked,
    )
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

#[derive(Clone)]
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
    if let ExecutionOutcome::StoppedWithEvidence { .. } = outcome {
        return outcome;
    }
    let effect = match outcome {
        ExecutionOutcome::Completed { effect, .. }
        | ExecutionOutcome::AtomicCompleted { effect, .. }
        | ExecutionOutcome::Stopped { effect } => effect,
        ExecutionOutcome::StoppedWithEvidence { .. } => unreachable!(),
    };
    ExecutionOutcome::Stopped { effect }
}

enum PhysicalAttempt {
    Terminal(ExecutionOutcome<RuntimeResult>),
    TryNextStrategy,
}

struct TextExecutionTarget {
    window: Option<WindowRef>,
    element: Option<xenoteer_protocol::ElementRef>,
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
        TextTarget::Window { window } => TextExecutionTarget {
            window: Some(window),
            element: None,
        },
        TextTarget::Element {
            element,
            window_fallback,
        } => TextExecutionTarget {
            window: window_fallback,
            element: Some(*element),
        },
    };
    match command.strategy {
        TextStrategy::Semantic => {
            let Some(element) = target.element else {
                return completed(invalid_clipboard_request());
            };
            let Some(options) = command.semantic_options else {
                return completed(invalid_clipboard_request());
            };
            execute_semantic_text(runtime, element, text, options, context).await
        }
        TextStrategy::Physical => {
            let Some(target) = target.window else {
                return completed(invalid_clipboard_request());
            };
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
                PhysicalAttempt::TryNextStrategy => completed(unsupported_text_strategy()),
            }
        }
        TextStrategy::PhysicalExtended => {
            let Some(target) = target.window else {
                return completed(invalid_clipboard_request());
            };
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
                PhysicalAttempt::TryNextStrategy => completed(unsupported_text_strategy()),
            }
        }
        TextStrategy::Clipboard => {
            let Some(target) = target.window else {
                return completed(invalid_clipboard_request());
            };
            let Some(options) = command.clipboard_options else {
                return completed(invalid_clipboard_request());
            };
            execute_clipboard_paste(runtime, command_id, target, text, options, context).await
        }
        TextStrategy::Auto => {
            if let Some(policy) = command.auto_policy {
                return execute_explicit_text_auto(
                    runtime,
                    command_id,
                    target,
                    text,
                    command.clipboard_options,
                    command.semantic_options,
                    policy.allowed_strategies,
                    context,
                )
                .await;
            }
            let Some(target) = target.window else {
                return completed(invalid_clipboard_request());
            };
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
                PhysicalAttempt::TryNextStrategy => {
                    execute_clipboard_paste(runtime, command_id, target, text, options, context)
                        .await
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_explicit_text_auto<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    command_id: CommandId,
    target: TextExecutionTarget,
    text: MaterializedText,
    clipboard_options: Option<TextInsertOptions>,
    semantic_options: Option<xenoteer_protocol::SemanticTextInsertOptions>,
    strategies: Vec<TextStrategy>,
    context: &mut C,
) -> ExecutionOutcome<RuntimeResult> {
    for (index, strategy) in strategies.iter().copied().enumerate() {
        let has_next = index + 1 < strategies.len();
        let attempt = match strategy {
            TextStrategy::Semantic => {
                let Some(element) = target.element.clone() else {
                    return completed(invalid_clipboard_request());
                };
                let Some(options) = semantic_options.clone() else {
                    return completed(invalid_clipboard_request());
                };
                execute_semantic_text(runtime, element, text.clone(), options, context).await
            }
            TextStrategy::Auto => {
                return completed(invalid_clipboard_request());
            }
            TextStrategy::Physical | TextStrategy::PhysicalExtended => {
                let Some(target) = target.window.clone() else {
                    return completed(invalid_clipboard_request());
                };
                let mode = if strategy == TextStrategy::Physical {
                    PhysicalTextMode::CurrentLayout
                } else {
                    PhysicalTextMode::ExtendedTemporaryMapping
                };
                match execute_physical_text(
                    runtime,
                    command_id,
                    target,
                    text.clone(),
                    mode,
                    has_next,
                    context,
                )
                .await
                {
                    PhysicalAttempt::TryNextStrategy => continue,
                    PhysicalAttempt::Terminal(outcome) => outcome,
                }
            }
            TextStrategy::Clipboard => {
                let Some(target) = target.window.clone() else {
                    return completed(invalid_clipboard_request());
                };
                let Some(options) = clipboard_options else {
                    return completed(invalid_clipboard_request());
                };
                execute_clipboard_paste(runtime, command_id, target, text.clone(), options, context)
                    .await
            }
        };
        if has_next && text_attempt_can_fallback(&attempt) {
            continue;
        }
        return attempt;
    }
    completed(capability_unavailable())
}

async fn execute_semantic_text<
    R: ClipboardExecutionRuntime + ?Sized,
    C: ClipboardExecutionContext + ?Sized,
>(
    runtime: &R,
    element: xenoteer_protocol::ElementRef,
    text: MaterializedText,
    options: xenoteer_protocol::SemanticTextInsertOptions,
    context: &mut C,
) -> ExecutionOutcome<RuntimeResult> {
    if context.stop_reason() != ExecutionStop::Continue {
        return ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        };
    }
    let deadline = context
        .deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let Some(mut operation) =
        runtime.semantic_text_insert(element, text.value, options, deadline, cancellation.clone())
    else {
        return completed(capability_unavailable());
    };
    let result = tokio::select! {
        result = &mut operation => Some(result),
        _reason = context.wait_for_stop() => None,
    };
    let Some(result) = result else {
        cancellation.cancel();
        let effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut operation).await {
            Ok(Ok(_)) => CommandEffect::AfterEffect,
            Ok(Err(error)) if error.effect_may_have_occurred() => CommandEffect::AfterEffect,
            Ok(Err(_)) => CommandEffect::BeforeEffect,
            Err(_) => CommandEffect::AfterEffect,
        };
        return ExecutionOutcome::Stopped { effect };
    };
    let semantic = match result {
        Ok(semantic) => semantic,
        Err(error) => return completed(map_semantic_failure(error)),
    };
    let exact_delta = semantic
        .character_count_after
        .checked_sub(semantic.character_count_before)
        == u32::try_from(text.unicode_scalars).ok();
    if !exact_delta {
        return completed(semantic_postcondition_failed());
    }
    let stage = if semantic.postcondition_satisfied == Some(true) {
        EffectStage::PostconditionMet
    } else {
        EffectStage::TextInserted
    };
    let evidence = TextInsertEvidence {
        selected_strategy: TextStrategy::Semantic,
        utf8_bytes: text.utf8_bytes,
        unicode_scalars: text.unicode_scalars,
        completed_scalars: text.unicode_scalars,
        clipboard: None,
        semantic: Some(semantic),
    };
    if evidence.validate().is_err() {
        return completed(backend_failure(EffectStage::OutcomeUnknown));
    }
    completed(RuntimeResult::success(
        CommandOutcome::TextInserted { evidence },
        stage,
    ))
}

fn text_attempt_can_fallback(outcome: &ExecutionOutcome<RuntimeResult>) -> bool {
    matches!(
        outcome,
        ExecutionOutcome::Completed {
            output: RuntimeResult::Failure(RuntimeFailure {
                code: ErrorCode::CapabilityUnavailable | ErrorCode::UnsupportedByTarget,
                ..
            }),
            effect: CommandEffect::BeforeEffect,
        }
    )
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
            semantic: None,
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
            PhysicalAttempt::TryNextStrategy
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
            PhysicalAttempt::TryNextStrategy
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
        semantic: None,
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
        semantic: None,
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

    #[cfg(test)]
    pub(crate) fn execute_window_control_for_test(
        &self,
        command: Command,
    ) -> Option<WindowControlResult> {
        match self.execute(command) {
            RuntimeResult::Success(success) => match success.outcome {
                CommandOutcome::WindowControl { result } => Some(result),
                _ => None,
            },
            RuntimeResult::Failure(_) => None,
        }
    }

    fn execute_cancellable(
        &self,
        command: Command,
        stop_requested: Arc<AtomicBool>,
    ) -> RuntimeResult {
        self.execute_fenced(command, WindowMutationFence::requested(stop_requested))
    }

    fn execute_fenced(&self, command: Command, fence: WindowMutationFence) -> RuntimeResult {
        if fence.is_stopped() {
            return backend_failure(EffectStage::None);
        }
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
        if fence.is_stopped() {
            return backend_failure(EffectStage::None);
        }

        let observation = Arc::clone(&self.observation);
        let revalidation_fence = fence;
        let revalidate_target = target.clone();
        let revalidate_sibling = sibling.clone();
        let reply_timeout = WINDOW_CONTROL_TIMEOUT
            .checked_add(WINDOW_REVALIDATION_TIMEOUT)
            .and_then(|timeout| timeout.checked_add(WINDOW_REVALIDATION_TIMEOUT))
            .and_then(|timeout| timeout.checked_add(WINDOW_CONTROL_REPLY_BACKSTOP))
            .unwrap_or(MAX_WINDOW_CONTROL_TIMEOUT);
        let evidence = match self.actor.execute(
            raw_request.clone(),
            Box::new(move || {
                revalidate_at_window_effect_boundary(&revalidation_fence, || {
                    revalidate_window(&observation, revalidate_target)?;
                    if let Some(sibling) = revalidate_sibling {
                        revalidate_window(&observation, sibling)?;
                    }
                    Ok(())
                })
            }),
            reply_timeout,
        ) {
            Ok(evidence) => evidence,
            Err(WindowControlBackendError::Submit(error)) => {
                return map_window_submit_error(error);
            }
            Err(WindowControlBackendError::Actor(failure)) => {
                tracing::debug!(
                    failure = ?failure,
                    "window-control actor request failed"
                );
                return map_window_actor_failure(failure);
            }
            Err(WindowControlBackendError::ReplyUnavailable) => {
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
        let post_effect = if matches!(
            &command,
            Command::WindowSetState(_) | Command::WindowMinimize(_)
        ) {
            self.observation
                .refresh_exact_blocking(target, WINDOW_REVALIDATION_TIMEOUT)
                .ok()
        } else {
            self.observation
                .snapshot_exact_blocking(target, WINDOW_REVALIDATION_TIMEOUT)
                .ok()
        };
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
    audience: EventAudience,
    desktop_id: DesktopId,
    generation: GenerationToken,
    last_global_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventAudience {
    principal: PrincipalId,
    allow_accessibility: bool,
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
                        match project_event(self.desktop_id, &self.audience, record) {
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
    audience: &EventAudience,
    record: EventRecord<RuntimeEvent>,
) -> Result<ProjectedEvent, ControlPlaneError> {
    let event = match record.event {
        RuntimeEvent::Targeted {
            audience: principal,
            event,
        } => {
            if principal != audience.principal {
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
    if event.topic.as_str().starts_with("accessibility.") && !audience.allow_accessibility {
        return Ok(ProjectedEvent::Hidden);
    }
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
    audience: &EventAudience,
    generation: GenerationToken,
    latest_sequence: u64,
    events: Vec<EventRecord<RuntimeEvent>>,
) -> Result<EventReplay, ControlPlaneError> {
    let mut projected = Vec::new();
    for record in events {
        match project_event(desktop_id, audience, record)? {
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
            let trace = output.trace().cloned();
            let result = match output {
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
            }?;
            return match trace {
                Some(trace) => result
                    .with_trace(trace)
                    .map_err(|_| ControlPlaneError::Internal),
                None => Ok(result),
            };
        }

        let (lifecycle, failure) = terminal_failure(terminal, generation);
        let trace = terminal
            .output
            .as_ref()
            .and_then(RuntimeResult::trace)
            .cloned();
        let problem = runtime_problem(&failure, generation)?;
        let result =
            if failure.effect_stage.has_visible_effect() || lifecycle == CommandLifecycle::Failed {
                accepted
                    .start(accepted_at)
                    .and_then(|running| running.fail(lifecycle, problem, finished_at))
            } else {
                accepted.fail(lifecycle, problem, finished_at)
            };
        let result = result.map_err(|_| ControlPlaneError::Internal)?;
        match trace {
            Some(trace) => result
                .with_trace(trace)
                .map_err(|_| ControlPlaneError::Internal),
            None => Ok(result),
        }
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
                detailed_trace: detailed_trace_requested(envelope.trace_policy),
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
            let audience = EventAudience {
                principal,
                allow_accessibility: context.principal().has_grant(Grant::AccessibilityRead),
            };
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
                    &audience,
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
                    audience,
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
                    trace_progress: RuntimeTraceProgress::None,
                    trace: None,
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
                    trace_progress: RuntimeTraceProgress::None,
                    trace: None,
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
                trace_progress: RuntimeTraceProgress::None,
                trace: None,
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
                trace_progress: RuntimeTraceProgress::None,
                trace: None,
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
                trace_progress: RuntimeTraceProgress::None,
                trace: None,
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
                trace_progress: RuntimeTraceProgress::None,
                trace: None,
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

const fn detailed_trace_requested(policy: Option<TracePolicy>) -> bool {
    matches!(policy, Some(TracePolicy::Detailed))
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
    Geometry(#[from] xenoteer_protocol::GeometryError),
    #[error(transparent)]
    AccessibilityCorrelation(#[from] AccessibilityCorrelationCoordinatorError),
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
    #[error("accessibility correlation task panicked")]
    AccessibilityCorrelationTaskPanicked,
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
    #[error("process event relay cancelled while correlation authority was fenced")]
    Cancelled,
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
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tokio::sync::Semaphore;
    use tower::ServiceExt;
    use xenoteer_protocol::{
        AccessibilityIdentityHash, AccessibilityRevision, ApplicationRef, AtspiBusName,
        AtspiGeneration, AtspiObjectPath, DesktopProbeCommand, ElementClickPointPolicy,
        ElementPostcondition, ElementRef, ElementStringMatch, ElementWaitPredicate, LaunchId,
        OcclusionCheckResult, ProcessRef, ProcessStatusCommand, RequestId, WindowIdentityHash,
    };
    use xenoteer_server::{
        AllowedOrigins, Authentication, DesktopReadiness, Principal, ReadinessHandle,
        ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
        api_router_with_control,
    };

    use super::*;

    #[derive(Default)]
    struct CountingProcessCorrelationInvalidator {
        calls: AtomicUsize,
    }

    impl ProcessCorrelationInvalidator for CountingProcessCorrelationInvalidator {
        fn disable(&self) {}

        fn enable(&self) {}

        fn invalidate<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), ControlPlaneError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Default)]
    struct LifecycleAuthorityProbe {
        enabled: AtomicBool,
        enable_calls: AtomicUsize,
        disable_calls: AtomicUsize,
        pending_invalidation: AtomicBool,
        cancel_on_enable: Mutex<Option<CancellationToken>>,
    }

    impl ProcessCorrelationInvalidator for LifecycleAuthorityProbe {
        fn disable(&self) {
            self.enabled.store(false, Ordering::Release);
            self.disable_calls.fetch_add(1, Ordering::AcqRel);
        }

        fn enable(&self) {
            self.enabled.store(true, Ordering::Release);
            self.enable_calls.fetch_add(1, Ordering::AcqRel);
            if let Some(cancellation) = self
                .cancel_on_enable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                cancellation.cancel();
            }
        }

        fn invalidate<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), ControlPlaneError>> + Send + 'a>> {
            if self.pending_invalidation.load(Ordering::Acquire) {
                Box::pin(std::future::pending())
            } else {
                Box::pin(async { Ok(()) })
            }
        }
    }

    struct ScriptedProcessEventSource {
        subscriptions: Mutex<VecDeque<Result<ProcessEventSubscription, ProcessEventSourceError>>>,
        calls: AtomicUsize,
    }

    impl ScriptedProcessEventSource {
        fn new(
            subscriptions: impl IntoIterator<
                Item = Result<ProcessEventSubscription, ProcessEventSourceError>,
            >,
        ) -> Self {
            Self {
                subscriptions: Mutex::new(subscriptions.into_iter().collect()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ProcessEventSource for ScriptedProcessEventSource {
        fn subscribe<'a>(&'a self, _: DesktopGeneration, _: u64) -> ProcessEventSourceFuture<'a> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let result = self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            Box::pin(async move {
                match result {
                    Some(result) => result,
                    None => std::future::pending().await,
                }
            })
        }
    }

    struct ScriptedProcessLiveEvents {
        events: VecDeque<Result<BrokerLiveEvent, ProcessEventSourceError>>,
    }

    impl ProcessLiveEventSource for ScriptedProcessLiveEvents {
        fn receive<'a>(&'a mut self) -> ProcessLiveEventFuture<'a> {
            let result = self.events.pop_front();
            Box::pin(async move {
                match result {
                    Some(result) => result,
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn scripted_subscription(
        replay: BrokerEventReplay,
        events: impl IntoIterator<Item = Result<BrokerLiveEvent, ProcessEventSourceError>>,
    ) -> ProcessEventSubscription {
        ProcessEventSubscription {
            replay,
            live: Box::new(ScriptedProcessLiveEvents {
                events: events.into_iter().collect(),
            }),
        }
    }

    #[test]
    fn process_relay_authority_guard_disables_on_normal_exit() {
        let probe = LifecycleAuthorityProbe::default();
        let cancellation = CancellationToken::new();
        {
            let guard = ProcessCorrelationAuthorityGuard::new(Some(&probe), &cancellation);
            assert!(guard.enable_if_live().is_ok());
            assert!(probe.enabled.load(Ordering::Acquire));
        }
        assert!(!probe.enabled.load(Ordering::Acquire));
        assert!(probe.disable_calls.load(Ordering::Acquire) >= 2);
    }

    #[test]
    fn cancellation_racing_replay_or_live_enable_cannot_reopen_authority() {
        let probe = LifecycleAuthorityProbe::default();
        let cancellation = CancellationToken::new();
        *probe
            .cancel_on_enable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancellation.clone());
        let guard = ProcessCorrelationAuthorityGuard::new(Some(&probe), &cancellation);
        assert!(matches!(
            guard.enable_if_live(),
            Err(ProcessEventRelayError::Cancelled)
        ));
        assert!(!probe.enabled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancellation_during_invalidation_stays_fenced_and_returns_promptly()
    -> Result<(), Box<dyn std::error::Error>> {
        let probe = Arc::new(LifecycleAuthorityProbe::default());
        probe.pending_invalidation.store(true, Ordering::Release);
        let cancellation = CancellationToken::new();
        let task_probe = Arc::clone(&probe);
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            invalidate_process_correlations(Some(task_probe.as_ref()), &task_cancellation).await
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_millis(50), task).await??;
        assert!(matches!(result, Err(ProcessEventRelayError::Cancelled)));
        assert!(!probe.enabled.load(Ordering::Acquire));
        Ok(())
    }

    #[tokio::test]
    async fn relay_recovers_only_after_clean_handoffs_and_exits_disabled()
    -> Result<(), Box<dyn std::error::Error>> {
        let generation = DesktopGeneration::new();
        let gap: BrokerProcessEvent = serde_json::from_value(serde_json::json!({
            "sequence": 2,
            "principal_id": "alice",
            "payload": process_exit_payload(generation, false, false)?,
        }))?;
        let malformed: BrokerProcessEvent = serde_json::from_value(serde_json::json!({
            "sequence": 3,
            "principal_id": "alice",
            "payload": process_exit_payload(DesktopGeneration::new(), false, false)?,
        }))?;
        let ambiguous_replay = scripted_subscription(
            BrokerEventReplay::Events {
                latest_sequence: 4,
                events: vec![gap, malformed],
            },
            [],
        );
        let clean_closed = scripted_subscription(
            BrokerEventReplay::Events {
                latest_sequence: 4,
                events: Vec::new(),
            },
            [Ok(BrokerLiveEvent::Closed)],
        );
        let clean_error = scripted_subscription(
            BrokerEventReplay::Events {
                latest_sequence: 4,
                events: Vec::new(),
            },
            [Err(ProcessEventSourceError::Injected)],
        );
        let clean_idle = scripted_subscription(
            BrokerEventReplay::Events {
                latest_sequence: 4,
                events: Vec::new(),
            },
            [],
        );
        let source = Arc::new(ScriptedProcessEventSource::new([
            Err(ProcessEventSourceError::Injected),
            Ok(ambiguous_replay),
            Ok(clean_closed),
            Ok(clean_error),
            Ok(clean_idle),
        ]));
        let authority = Arc::new(LifecycleAuthorityProbe::default());
        let cancellation = CancellationToken::new();
        let (sender, _receiver) = mpsc::channel(8);
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::new(AtomicU64::new(0)),
            resync_notify: Arc::new(Notify::new()),
        };
        let task_source = Arc::clone(&source);
        let task_authority: Arc<dyn ProcessCorrelationInvalidator> = authority.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            relay_process_events_inner(
                ingress,
                task_source.as_ref(),
                generation,
                task_cancellation,
                Some(task_authority),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            while source.calls.load(Ordering::Acquire) < 5
                || !authority.enabled.load(Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(authority.enable_calls.load(Ordering::Acquire), 3);
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task).await??;
        assert!(result.is_ok());
        assert!(!authority.enabled.load(Ordering::Acquire));
        Ok(())
    }

    #[tokio::test]
    async fn repeated_live_resync_boundaries_use_exponential_reconnect_backoff()
    -> Result<(), Box<dyn std::error::Error>> {
        let generation = DesktopGeneration::new();
        let subscriptions = (0_u64..4)
            .map(|cursor| {
                Ok(scripted_subscription(
                    BrokerEventReplay::Events {
                        latest_sequence: cursor,
                        events: Vec::new(),
                    },
                    [Ok(BrokerLiveEvent::ResyncRequired {
                        dropped_through: cursor + 1,
                        latest_sequence: cursor + 1,
                    })],
                ))
            })
            .chain(std::iter::once(Ok(scripted_subscription(
                BrokerEventReplay::Events {
                    latest_sequence: 4,
                    events: Vec::new(),
                },
                [],
            ))));
        let source = Arc::new(ScriptedProcessEventSource::new(subscriptions));
        let authority = Arc::new(LifecycleAuthorityProbe::default());
        let cancellation = CancellationToken::new();
        let (sender, _receiver) = mpsc::channel(8);
        let ingress = ExternalEventIngress {
            sender,
            resync_state: Arc::new(AtomicU64::new(0)),
            resync_notify: Arc::new(Notify::new()),
        };
        let task_source = Arc::clone(&source);
        let task_authority: Arc<dyn ProcessCorrelationInvalidator> = authority.clone();
        let task_cancellation = cancellation.clone();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            relay_process_events_inner(
                ingress,
                task_source.as_ref(),
                generation,
                task_cancellation,
                Some(task_authority),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            while source.calls.load(Ordering::Acquire) < 5
                || !authority.enabled.load(Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            started.elapsed() >= Duration::from_millis(650),
            "four immediate resyncs bypassed the expected 50/100/200/400ms backoff"
        );
        assert_eq!(source.calls.load(Ordering::Acquire), 5);
        assert_eq!(authority.enable_calls.load(Ordering::Acquire), 5);

        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task).await??;
        assert!(result.is_ok());
        assert!(!authority.enabled.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn principal_event_filter_hides_payloads_without_renumbering_global_gaps()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let alice = PrincipalId::new("alice")?;
        let bob = PrincipalId::new("bob")?;
        let alice_audience = EventAudience {
            principal: alice.clone(),
            allow_accessibility: false,
        };
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
            .map(|record| project_event(desktop_id, &alice_audience, record))
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
                    serde_json::json!({"model_revision": "17"}),
                )?,
            },
        };
        for principal in [PrincipalId::new("alice")?, PrincipalId::new("bob")?] {
            let audience = EventAudience {
                principal,
                allow_accessibility: false,
            };
            let projected = project_event(desktop_id, &audience, record.clone())
                .map_err(|_| std::io::Error::other("event projection failed"))?;
            assert!(matches!(
                projected,
                ProjectedEvent::Visible(SequencedEvent { sequence: 9, .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn accessibility_events_require_authority_in_daemon_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation =
            xenoteer_core::coordinator::GenerationFence::new(desktop_id, DesktopGeneration::new())
                .capture();
        let record = EventRecord {
            generation,
            sequence: 11,
            encoded_size: 128,
            event: RuntimeEvent::Broadcast {
                event: NormalizedEvent::new(
                    EventTopic::new("accessibility.element_changed")?,
                    serde_json::json!({"name": "private value"}),
                )?,
            },
        };
        let principal = PrincipalId::new("observer")?;
        let denied = EventAudience {
            principal: principal.clone(),
            allow_accessibility: false,
        };
        let allowed = EventAudience {
            principal,
            allow_accessibility: true,
        };

        assert_eq!(
            project_event(desktop_id, &denied, record.clone())
                .map_err(|_| std::io::Error::other("event projection failed"))?,
            ProjectedEvent::Hidden
        );
        assert!(matches!(
            project_event(desktop_id, &allowed, record)
                .map_err(|_| std::io::Error::other("event projection failed"))?,
            ProjectedEvent::Visible(SequencedEvent { sequence: 11, .. })
        ));
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
        ingress.try_broadcast(topic.clone(), serde_json::json!({"model_revision": "1"}))?;
        assert_eq!(
            ingress.try_broadcast(topic, serde_json::json!({"model_revision": "2"})),
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
            ingress.try_broadcast(topic.clone(), serde_json::json!({"model_revision": "1"})),
            Err(ExternalEventIngressError::Full)
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(claim_external_resync(&resync_state), Some(2));
        assert_eq!(claim_external_resync(&resync_state), None);

        ingress.try_broadcast(topic, serde_json::json!({"model_revision": "2"}))?;
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
                    serde_json::json!({"model_revision": "7"}),
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
            serde_json::json!({"model_revision": "7"}),
        )?;

        let generation = DesktopGeneration::new();
        let process_event: BrokerProcessEvent = serde_json::from_value(serde_json::json!({
            "sequence": 2,
            "principal_id": "alice",
            "payload": process_exit_payload(generation, false, false)?,
        }))?;
        let invalidator = CountingProcessCorrelationInvalidator::default();
        let outcome = relay_process_event(
            &ingress,
            generation,
            &CancellationToken::new(),
            0,
            process_event,
            Some(&invalidator),
        )
        .await?;
        assert_eq!(outcome, ProcessRelayEventOutcome::Ambiguous { cursor: 2 });
        assert_eq!(invalidator.calls.load(Ordering::Acquire), 1);
        assert_eq!(resync_state.load(Ordering::Acquire), 1);

        let claimed_epoch = claim_external_resync(&resync_state).ok_or("gap was not claimed")?;
        assert_eq!(claimed_epoch, 2);
        assert!(event_after_external_barrier(receiver.try_recv()?, claimed_epoch).is_none());
        assert!(receiver.try_recv().is_err());

        ingress.try_broadcast(
            EventTopic::new("window.changed")?,
            serde_json::json!({"model_revision": "8"}),
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
                    serde_json::json!({"model_revision": "9"}),
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
            &EventAudience {
                principal: PrincipalId::new("unrelated-observer")?,
                allow_accessibility: false,
            },
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
                accessibility_correlation_join: None,
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

        let mut no_trace = first.clone();
        no_trace.trace_policy = Some(TracePolicy::None);
        let mut detailed = first.clone();
        detailed.trace_policy = Some(TracePolicy::Detailed);
        assert_ne!(test_hash(&first)?, test_hash(&no_trace)?);
        assert_ne!(test_hash(&no_trace)?, test_hash(&detailed)?);
        Ok(())
    }

    #[test]
    fn detailed_trace_distinguishes_semantic_from_physical_stages()
    -> Result<(), Box<dyn std::error::Error>> {
        let semantic = RuntimeResult::success(
            CommandOutcome::Acknowledged,
            EffectStage::SemanticStateChanged,
        );
        let semantic_trace = detailed_runtime_trace(&semantic, RuntimeTraceIntent::Semantic)?;
        assert_eq!(
            semantic_trace.domain,
            CommandTraceDomain::SemanticAccessibility
        );
        assert!(semantic_trace.steps.iter().any(|step| {
            step.stage == CommandTraceStage::SemanticDispatched
                && step.status == CommandTraceStatus::Completed
        }));
        assert!(
            semantic_trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticReadback)
        );
        assert!(!semantic_trace.steps.iter().any(|step| matches!(
            step.stage,
            CommandTraceStage::PhysicalButtonPress | CommandTraceStage::PhysicalButtonRelease
        )));

        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let atspi_generation = AtspiGeneration::new(1)?;
        let application = ApplicationRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            unique_bus_name: AtspiBusName::new(":1.42")?,
            root_object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/root")?,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
        };
        let element = ElementRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            application,
            object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/button")?,
            object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))?,
            cache_sequence: 3,
        };
        let physical = RuntimeResult::success(
            CommandOutcome::ElementPhysicalClick {
                result: ElementPhysicalClickResult {
                    element,
                    window: WindowRef {
                        desktop_id,
                        desktop_generation,
                        xid: 42,
                        observed_generation: 1,
                        identity_hash: WindowIdentityHash::new("c".repeat(64))?,
                    },
                    correlation: xenoteer_protocol::WindowCorrelationConfidence::Strong,
                    revision_before_queue: AccessibilityRevision::new(3)?,
                    revision_after_queue: AccessibilityRevision::new(4)?,
                    extents_before_queue: Rect::new(10, 10, 100, 30)?,
                    extents_after_queue: Rect::new(10, 10, 100, 30)?,
                    click_point: Point::new(60, 25),
                    occlusion_check: OcclusionCheckResult::NotRequested,
                    scrolled: false,
                    window_activated: true,
                    pointer_interpolated: true,
                    button: PointerLogicalButton::Left,
                    count: 1,
                    postcondition_satisfied: None,
                    final_snapshot: None,
                },
            },
            EffectStage::ElementPhysicallyClicked,
        );
        let physical_trace =
            detailed_runtime_trace(&physical, RuntimeTraceIntent::PhysicalElement)?;
        assert_eq!(
            physical_trace.domain,
            CommandTraceDomain::PhysicalElementInput
        );
        for stage in [
            CommandTraceStage::PhysicalCorrelationRevalidated,
            CommandTraceStage::PhysicalWindowRevalidated,
            CommandTraceStage::PhysicalScroll,
            CommandTraceStage::PhysicalActivation,
            CommandTraceStage::PhysicalPointerInterpolation,
            CommandTraceStage::PhysicalButtonPress,
            CommandTraceStage::PhysicalButtonRelease,
        ] {
            assert!(physical_trace.steps.iter().any(|step| step.stage == stage));
        }
        assert!(
            !physical_trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticDispatched)
        );
        Ok(())
    }

    #[test]
    fn normal_and_none_trace_policy_omit_inline_trace() {
        assert!(!detailed_trace_requested(None));
        assert!(!detailed_trace_requested(Some(TracePolicy::None)));
        assert!(!detailed_trace_requested(Some(TracePolicy::Normal)));
        assert!(detailed_trace_requested(Some(TracePolicy::Detailed)));
    }

    #[test]
    fn detailed_failure_trace_reports_only_observed_semantic_progress()
    -> Result<(), Box<dyn std::error::Error>> {
        let before_dispatch = map_semantic_failure(SemanticActionFailure::Disabled);
        let before_trace = detailed_runtime_trace(&before_dispatch, RuntimeTraceIntent::Semantic)?;
        assert!(
            !before_trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticDispatched)
        );

        let after_readback = map_semantic_failure(SemanticActionFailure::PostconditionFailed);
        let after_trace = detailed_runtime_trace(&after_readback, RuntimeTraceIntent::Semantic)?;
        assert!(
            after_trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticDispatched)
        );
        assert!(
            after_trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticReadback)
        );
        assert_eq!(
            after_trace.steps.last().map(|step| step.status),
            Some(CommandTraceStatus::Failed)
        );
        Ok(())
    }

    #[test]
    fn detailed_stopped_outcome_retains_bounded_trace_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let stopped = attach_detailed_trace(
            ExecutionOutcome::Stopped {
                effect: CommandEffect::AfterEffect,
            },
            RuntimeTraceIntent::Semantic,
        );
        let ExecutionOutcome::StoppedWithEvidence { output, effect } = stopped else {
            return Err("detailed stopped outcome did not retain evidence".into());
        };
        assert_eq!(effect, CommandEffect::AfterEffect);
        let trace = output
            .trace()
            .ok_or("detailed stopped outcome omitted its trace")?;
        assert_eq!(trace.domain, CommandTraceDomain::SemanticAccessibility);
        assert!(
            trace
                .steps
                .iter()
                .any(|step| step.stage == CommandTraceStage::SemanticDispatched)
        );
        assert_eq!(
            trace.steps.last().map(|step| step.status),
            Some(CommandTraceStatus::Failed)
        );
        assert!(trace.steps.len() <= xenoteer_protocol::MAX_COMMAND_TRACE_STEPS);
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
                output: RuntimeResult::Success(success),
                ..
            } if matches!(
                &success.outcome,
                CommandOutcome::ApplicationLaunched { process: returned } if returned == &process
            )
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
                output: RuntimeResult::Success(success),
                ..
            } if matches!(
                &success.outcome,
                CommandOutcome::ProcessTerminated { process: returned } if returned == &view
            )
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

    #[test]
    fn semantic_success_stages_distinguish_dispatch_state_text_and_postcondition() {
        use xenoteer_protocol::ElementActionOperation;

        assert_eq!(
            semantic_success_stage(ElementActionOperation::Invoke, false),
            EffectStage::SemanticActionDispatched
        );
        assert_eq!(
            semantic_success_stage(ElementActionOperation::Scroll, false),
            EffectStage::SemanticActionDispatched
        );
        for operation in [
            ElementActionOperation::Focus,
            ElementActionOperation::SetValue,
            ElementActionOperation::Selection,
            ElementActionOperation::SetText,
        ] {
            assert_eq!(
                semantic_success_stage(operation, false),
                EffectStage::SemanticStateChanged
            );
        }
        assert_eq!(
            semantic_success_stage(ElementActionOperation::InsertText, false),
            EffectStage::TextInserted
        );
        assert_eq!(
            semantic_success_stage(ElementActionOperation::Invoke, true),
            EffectStage::PostconditionMet
        );
    }

    #[test]
    fn semantic_failures_preserve_before_and_after_effect_boundaries() {
        let rejected = map_semantic_failure(SemanticActionFailure::BackendRejected);
        assert!(matches!(rejected, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(rejected) = rejected else {
            return;
        };
        assert_eq!(rejected.code, ErrorCode::UnsupportedByTarget);
        assert_eq!(rejected.effect_stage, EffectStage::SemanticActionDispatched);

        let queue_full = map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::QueueFull,
        ));
        assert!(matches!(queue_full, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(queue_full) = queue_full else {
            return;
        };
        assert_eq!(queue_full.code, ErrorCode::ResourceExhausted);
        assert_eq!(queue_full.effect_stage, EffectStage::None);

        let cancelled_after = map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::CancelledAfterDispatch,
        ));
        assert!(matches!(cancelled_after, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(cancelled_after) = cancelled_after else {
            return;
        };
        assert_eq!(cancelled_after.code, ErrorCode::CancelledAfterEffect);
        assert_eq!(
            cancelled_after.effect_stage,
            EffectStage::SemanticActionDispatched
        );

        let reply_lost = map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::ReplyLostAfterAdmission,
        ));
        assert!(matches!(reply_lost, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(reply_lost) = reply_lost else {
            return;
        };
        assert_eq!(reply_lost.code, ErrorCode::RequestOutcomeUnknown);
        assert_eq!(reply_lost.effect_stage, EffectStage::OutcomeUnknown);
    }

    #[tokio::test]
    async fn physical_click_rejects_unsupported_postconditions_before_any_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let atspi_generation = AtspiGeneration::new(1)?;
        let application = ApplicationRef {
            desktop_id,
            desktop_generation: generation,
            atspi_generation,
            unique_bus_name: AtspiBusName::new(":1.42")?,
            root_object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/root")?,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
        };
        let element = ElementRef {
            desktop_id,
            desktop_generation: generation,
            atspi_generation,
            application,
            object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/42")?,
            object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))?,
            cache_sequence: 7,
        };
        let (_input_sender, input) = tokio::sync::watch::channel(None::<InputActorHandle>);
        let executor = RuntimeExecutor {
            input,
            window_control: None,
            clipboard: None,
            accessibility: None,
            accessibility_correlation: None,
            broker: BrokerClient::new("/path/that/must/not/exist"),
            motion_policy: MotionPolicy::default(),
            desktop_id,
            generation,
            root_bounds: Rect::new(0, 0, 1_024, 768)?,
        };

        for predicate in [ElementWaitPredicate::Text {
            matcher: ElementStringMatch::Exact {
                value: "never-read".to_owned(),
                case_sensitive: true,
            },
        }] {
            let result = execute_physical_element_click_inner(
                &executor,
                CommandId::new(),
                ElementPhysicalClickCommand {
                    element: element.clone(),
                    window: None,
                    minimum_correlation: xenoteer_protocol::WindowCorrelationConfidence::Strong,
                    point_policy: ElementClickPointPolicy::Center,
                    scroll_policy: ElementClickScrollPolicy::Always,
                    activation_policy: ElementWindowActivationPolicy::Require,
                    occlusion_policy: ElementOcclusionPolicy::BestEffortReject,
                    button: PointerLogicalButton::Left,
                    count: 1,
                    interval_ms: 0,
                    move_duration_ms: Some(100),
                    curve: PointerCurve::Smooth,
                    settle_timeout_ms: 1_000,
                    postcondition: Some(ElementPostcondition {
                        predicate,
                        timeout_ms: 1_000,
                        allow_poll_fallback: false,
                    }),
                },
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await;
            let RuntimeResult::Failure(failure) = result else {
                return Err("unsupported physical postcondition unexpectedly succeeded".into());
            };
            assert_eq!(failure.code, ErrorCode::InvalidRequest);
            assert_eq!(failure.effect_stage, EffectStage::None);
            assert_eq!(failure.retry, RetryAdvice::Never);
        }
        Ok(())
    }

    #[test]
    fn text_auto_falls_back_only_for_explicit_pre_dispatch_semantic_failures() {
        let unsupported = completed(map_semantic_failure(
            SemanticActionFailure::VerificationUnsupported,
        ));
        assert!(text_attempt_can_fallback(&unsupported));

        let rejected = completed(map_semantic_failure(SemanticActionFailure::BackendRejected));
        assert!(!text_attempt_can_fallback(&rejected));

        let stale = completed(map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::StaleIdentity,
        )));
        assert!(!text_attempt_can_fallback(&stale));

        let after_effect = completed(map_semantic_failure(
            SemanticActionFailure::PostconditionFailed,
        ));
        assert!(!text_attempt_can_fallback(&after_effect));

        let unknown = completed(map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::ReplyLostAfterAdmission,
        )));
        assert!(!text_attempt_can_fallback(&unknown));
    }

    #[test]
    fn semantic_action_resolution_failures_use_specific_codes() {
        let not_found = map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::ActionNotFound,
        ));
        assert!(matches!(not_found, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(not_found) = not_found else {
            return;
        };
        assert_eq!(not_found.code, ErrorCode::ActionNotFound);
        assert_eq!(not_found.effect_stage, EffectStage::None);

        let ambiguous = map_semantic_failure(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::AmbiguousAction,
        ));
        assert!(matches!(ambiguous, RuntimeResult::Failure(_)));
        let RuntimeResult::Failure(ambiguous) = ambiguous else {
            return;
        };
        assert_eq!(ambiguous.code, ErrorCode::AmbiguousTarget);
        assert_eq!(ambiguous.effect_stage, EffectStage::None);
    }
}
