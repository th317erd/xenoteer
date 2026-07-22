//! Daemon composition for the coordinator, input actor, and process broker.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{broadcast, oneshot, watch},
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
        ExecutionOutcome, GenerationToken, LeaseError, LeasePolicy, LeaseRequirement,
        LeaseSnapshot, MonotonicMillis, PrincipalId, ReplayFailure, ReplayResult, ResetOutcome,
        ResetRequest, TerminalCause, spawn_coordinator_with_event_mapper,
    },
    domain::RootPoint,
    input::{
        ButtonDirection, InputAction, MotionCurve, MotionOptions, MotionPlanError, MotionPolicy,
        PhysicalButton,
    },
};
use xenoteer_processd::{
    BrokerClient, BrokerClientError, BrokerErrorCode, BrokerEventReplay, BrokerLiveEvent,
    BrokerProcessEvent, DEFAULT_BROKER_SOCKET,
};
use xenoteer_protocol::{
    ACTION_LIFECYCLE_TOPIC, COMMAND_LIFECYCLE_TOPIC, Command, CommandEnvelope, CommandId,
    CommandLifecycle, CommandOutcome, CommandResult, ControlLeaseId, DesktopGeneration, DesktopId,
    EffectStage, ErrorCode, EventResyncReason, EventTopic, LeaseAcquireRequest, LeaseAvailability,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, NormalizedEvent, PROCESS_EXITED_TOPIC,
    PointerCurve, Problem, ProcessExitedEvent, RetryAdvice, SequencedEvent, Timestamp,
};
use xenoteer_server::{
    CommandCancellation, CommandSubmission, CommandWait, ControlFuture, ControlPlane,
    ControlPlaneError, ControlRequestContext, EventReplay, EventSubscription, Grant, LiveEvent,
    LiveEventReceiver, SubmissionDisposition,
};
use xenoteer_x11::{
    input::{
        ActionContext, ControlOutcome, InputActorHandle, InputFailure, InputFailureKind,
        InputOutcome, InputOutcomeKind, InputSubmitError, KeyboardAction, PointerMoveRequest,
    },
    keyboard::KeyIdentifier,
};

const LEASE_TTL_MS: u64 = 60_000;
const MAX_CONCURRENT_EXECUTIONS: usize = 32;
const EVENT_RETENTION_COUNT: usize = 10_000;
const EVENT_RETENTION_BYTES: usize = 16 * 1024 * 1024;
const INPUT_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_EVENT_RECONNECT_INITIAL: Duration = Duration::from_millis(50);
const PROCESS_EVENT_RECONNECT_MAXIMUM: Duration = Duration::from_secs(2);
const RESYNC_BARRIER_RETENTION_CHARGE: usize = 64;

type RuntimeHandle = CoordinatorHandle<RuntimeCommand, RuntimeResult, RuntimeEvent>;

/// Owned coordinator task and its HTTP adapter.
pub(crate) struct CoordinatorRuntime {
    handle: RuntimeHandle,
    join: JoinHandle<()>,
    process_event_cancellation: CancellationToken,
    process_event_join: JoinHandle<Result<(), ProcessEventRelayError>>,
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
pub(crate) fn spawn(
    config: &Config,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    input: watch::Receiver<Option<InputActorHandle>>,
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
        broker: broker.clone(),
        motion_policy: MotionPolicy::from_input_config(config.input())?,
    };
    let event_mapper = RuntimeEventMapper::new()?;
    let (handle, join) = spawn_coordinator_with_event_mapper(settings, executor, event_mapper)?;
    let process_event_cancellation = CancellationToken::new();
    let relay_cancellation = process_event_cancellation.clone();
    let relay_handle = handle.clone();
    let process_event_join = tokio::spawn(async move {
        relay_process_events(
            relay_handle,
            broker,
            desktop_id,
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
        control,
    })
}

#[derive(Clone, Debug)]
struct RuntimeCommand {
    command_id: CommandId,
    principal: PrincipalId,
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

#[derive(Clone, Debug, PartialEq)]
enum RuntimeEvent {
    Targeted {
        audience: PrincipalId,
        event: NormalizedEvent,
    },
    ResyncBarrier,
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
    handle: RuntimeHandle,
    broker: BrokerClient,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    cancellation: CancellationToken,
) -> Result<(), ProcessEventRelayError> {
    let generation = handle.generation().await?;
    if generation.desktop_id() != desktop_id || generation.generation() != desktop_generation {
        return Err(ProcessEventRelayError::WrongGeneration);
    }
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
                        &handle,
                        generation,
                        desktop_generation,
                        &cancellation,
                        cursor,
                        event,
                    )
                    .await?;
                }
                if cursor != latest_sequence {
                    publish_resync_barrier(&handle, generation, &cancellation).await?;
                    cursor = latest_sequence;
                }
            }
            BrokerEventReplay::ResyncRequired {
                latest_sequence, ..
            } => {
                publish_resync_barrier(&handle, generation, &cancellation).await?;
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
                        &handle,
                        generation,
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
                    publish_resync_barrier(&handle, generation, &cancellation).await?;
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
    handle: &RuntimeHandle,
    generation: GenerationToken,
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
        publish_resync_barrier(handle, generation, cancellation).await?;
    }
    match normalize_process_event(desktop_generation, event) {
        Ok(event) => publish_runtime_event(handle, generation, cancellation, event).await?,
        Err(error) => {
            tracing::error!(error = %error, "invalid process event rejected");
            publish_resync_barrier(handle, generation, cancellation).await?;
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

async fn publish_runtime_event(
    handle: &RuntimeHandle,
    generation: GenerationToken,
    cancellation: &CancellationToken,
    event: RuntimeEvent,
) -> Result<(), ProcessEventRelayError> {
    let encoded_size = match &event {
        RuntimeEvent::Targeted { event, .. } => event
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
}

#[derive(Clone)]
struct RuntimeExecutor {
    input: watch::Receiver<Option<InputActorHandle>>,
    broker: BrokerClient,
    motion_policy: MotionPolicy,
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
        }
    }
}

fn input_context(command_id: CommandId, context: &ExecutionContext) -> ActionContext {
    ActionContext::new(command_id, context.deadline().map(Instant::into_std))
}

#[derive(Clone, Copy)]
enum InputStage {
    PointerMove,
    ButtonDown,
    ButtonUp,
    KeyDown,
    KeyUp,
}

impl InputStage {
    const fn after_effect(self) -> EffectStage {
        match self {
            Self::PointerMove => EffectStage::PointerMoved,
            Self::ButtonDown => EffectStage::ButtonPressed,
            Self::ButtonUp => EffectStage::ButtonReleased,
            Self::KeyDown => EffectStage::KeyPressed,
            Self::KeyUp => EffectStage::KeyReleased,
        }
    }
}

async fn await_input(
    receiver: Result<oneshot::Receiver<Result<InputOutcome, InputFailure>>, InputSubmitError>,
    mut context: ExecutionContext,
    cancellation: CancellationToken,
    stage: InputStage,
) -> ExecutionOutcome<RuntimeResult> {
    let mut receiver = match receiver {
        Ok(receiver) => receiver,
        Err(InputSubmitError::QueueFull) => return completed(resource_exhausted()),
        Err(InputSubmitError::Closed) => return completed(capability_unavailable()),
    };
    tokio::select! {
        result = &mut receiver => map_input_result(result, stage),
        _reason = context.wait_for_stop() => {
            cancellation.cancel();
            let effect = match tokio::time::timeout(INPUT_CONTROL_TIMEOUT, &mut receiver).await {
                Ok(Ok(Ok(outcome))) if outcome.events_emitted > 0 => CommandEffect::AfterEffect,
                Ok(Ok(Err(failure))) if failure.events_emitted > 0 || !failure.progress_known => {
                    CommandEffect::AfterEffect
                }
                Err(_) => CommandEffect::AfterEffect,
                _ => CommandEffect::BeforeEffect,
            };
            ExecutionOutcome::Stopped { effect }
        }
    }
}

fn map_input_result(
    result: Result<Result<InputOutcome, InputFailure>, oneshot::error::RecvError>,
    stage: InputStage,
) -> ExecutionOutcome<RuntimeResult> {
    match result {
        Ok(Ok(outcome)) => {
            let effect_stage = if outcome.events_emitted == 0 {
                EffectStage::None
            } else {
                stage.after_effect()
            };
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
        Ok(Err(failure)) => completed(input_failure(failure, stage)),
        Err(_) => completed(backend_failure(EffectStage::SideEffectObserved)),
    }
}

fn input_failure(failure: InputFailure, stage: InputStage) -> RuntimeResult {
    let effect_stage = if failure.events_emitted > 0 || !failure.progress_known {
        stage.after_effect()
    } else {
        EffectStage::None
    };
    match failure.kind {
        InputFailureKind::CancelledBeforeEffect => RuntimeResult::failure(
            409,
            ErrorCode::CancelledBeforeEffect,
            "Command cancelled before effect",
            "Cancellation was observed before physical input changed the desktop.",
            RetryAdvice::SameCommandId,
            EffectStage::None,
        ),
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
        _ => backend_failure(effect_stage),
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
    RuntimeResult::failure(
        503,
        ErrorCode::CapabilityUnavailable,
        "Desktop capability unavailable",
        "The required desktop subsystem is not ready.",
        RetryAdvice::AfterBackoff,
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
    let RuntimeEvent::Targeted { audience, event } = record.event else {
        return Ok(ProjectedEvent::ResyncBarrier {
            sequence: record.sequence,
        });
    };
    if audience != *principal {
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

const fn required_command_grant(command: &Command) -> Grant {
    match command {
        Command::PointerMove(_)
        | Command::PointerButtonDown(_)
        | Command::PointerButtonUp(_)
        | Command::KeyboardKeyDown(_)
        | Command::KeyboardKeyUp(_)
        | Command::InputReset(_) => Grant::InputControl,
        Command::ApplicationLaunch(_) => Grant::ApplicationLaunch,
        Command::ProcessTerminate(_) => Grant::ApplicationTerminate,
        Command::DesktopProbe(_) | Command::ProcessStatus(_) => Grant::DesktopObserve,
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
        [
            Grant::InputControl,
            Grant::ApplicationLaunch,
            Grant::ApplicationTerminate,
        ]
        .into_iter()
        .any(|grant| context.principal().has_grant(grant))
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
            Self::require_grant(&context, required_command_grant(&envelope.command))?;
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
}

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
            let runtime = CoordinatorRuntime {
                handle,
                join,
                process_event_cancellation: CancellationToken::new(),
                process_event_join: tokio::spawn(async { Ok(()) }),
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
