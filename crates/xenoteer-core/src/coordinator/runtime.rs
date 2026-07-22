//! Async serialization boundary for coordinator state machines.
//!
//! The task in this module is the sole mutable owner of the generation fence,
//! lease transaction, command ledger, and event replay hub. Transport adapters
//! interact through [`CoordinatorHandle`]; backend effects are isolated behind
//! [`CommandExecutor`].

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_util::FutureExt;
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use xenoteer_protocol::{CommandId, ControlLeaseId, DesktopGeneration, DesktopId};

use super::{
    CanonicalCommandHash, CommandLedger, CommandLedgerError, CommandLedgerLimits, CommandRecord,
    CommandRecordState, EventHub, EventHubError, EventHubLimits, GenerationFence,
    GenerationFenceError, GenerationToken, IdempotencyDecision, LeaseError, LeaseGrant,
    LeaseMachine, LeasePhase, LeasePolicy, LeaseSnapshot, MonotonicMillis, PrincipalId,
    ReplayResult, RevocationReason,
};

/// Type-erased future returned by a coordinator backend.
pub type BoxCoordinatorFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Handle and task returned by [`spawn_coordinator`].
pub type SpawnedCoordinator<C, R, E> = (CoordinatorHandle<C, R, E>, JoinHandle<()>);

type CommandWatch<R> = watch::Receiver<CommandRecord<CommandTerminal<R>>>;
type CommandWatchReply<R> = oneshot::Sender<Result<Option<CommandWatch<R>>, CoordinatorError>>;
// Live fanout stores sequence notifications, never cloned payloads. Four covers
// the actor's bounded accepted/running/terminal burst; a larger flood makes a
// slow receiver explicitly resynchronize from authoritative snapshots.
const LIVE_EVENT_FANOUT_CAPACITY: usize = 4;

/// Race-free retained replay plus bounded live delivery from one actor point.
///
/// The coordinator installs `live` before it snapshots `replay`. Events
/// published after that actor operation are therefore either present in the
/// replay suffix or delivered by the receiver. A lagged receiver must be
/// treated as an explicit resynchronization boundary by its transport.
#[derive(Debug)]
pub struct EventSubscription<E> {
    /// Complete retained suffix, or an explicit reason snapshots are required.
    pub replay: ReplayResult<E>,
    /// Bounded shared live ring. `Lagged` means completeness was lost.
    pub live: broadcast::Receiver<u64>,
}

/// One normalized coordinator-owned event ready for sequence assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorEvent<E> {
    /// Topic/payload data owned by the embedding control plane.
    pub event: E,
    /// Conservative encoded wire charge applied to bounded retention.
    pub encoded_size: usize,
}

/// Maps actor-owned command transitions to normalized events exactly once.
///
/// The mapper runs only at the accepted, running, and terminal mutation points;
/// transports never synthesize lifecycle events per connection.
pub trait CommandEventMapper<R, E>: Send + Sync + 'static {
    /// Returns a small bounded set of normalized events for one transition.
    fn map_command_transition(
        &self,
        principal: &PrincipalId,
        record: &CommandRecord<CommandTerminal<R>>,
    ) -> Vec<CoordinatorEvent<E>>;
}

#[derive(Debug, Clone, Copy)]
struct NoCommandEvents;

impl<R, E> CommandEventMapper<R, E> for NoCommandEvents {
    fn map_command_transition(
        &self,
        _principal: &PrincipalId,
        _record: &CommandRecord<CommandTerminal<R>>,
    ) -> Vec<CoordinatorEvent<E>> {
        Vec::new()
    }
}

/// Whether execution crossed an externally visible effect boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEffect {
    /// Execution stopped before an externally visible effect.
    BeforeEffect,
    /// Execution stopped after at least one externally visible effect.
    AfterEffect,
}

/// Authoritative reason supplied to a cooperative backend execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStop {
    /// No stop has been requested.
    Continue,
    /// The authenticated owner explicitly cancelled the command.
    Cancelled,
    /// The command's monotonic deadline elapsed.
    DeadlineExceeded,
    /// The desktop lifetime changed while the command was running.
    GenerationChanged,
    /// The coordinator is shutting down.
    Shutdown,
}

/// Context supplied to exactly one admitted backend execution.
#[derive(Debug)]
pub struct ExecutionContext {
    generation: GenerationToken,
    deadline: Option<Instant>,
    stop: watch::Receiver<ExecutionStop>,
}

impl ExecutionContext {
    /// Returns the desktop generation captured at admission.
    #[must_use]
    pub const fn generation(&self) -> GenerationToken {
        self.generation
    }

    /// Returns the monotonic execution deadline, when one was supplied.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns the currently requested stop reason without waiting.
    #[must_use]
    pub fn stop_reason(&self) -> ExecutionStop {
        *self.stop.borrow()
    }

    /// Waits until the coordinator requests a stop or closes.
    pub async fn wait_for_stop(&mut self) -> ExecutionStop {
        loop {
            let reason = *self.stop.borrow_and_update();
            if reason != ExecutionStop::Continue {
                return reason;
            }
            if self.stop.changed().await.is_err() {
                return ExecutionStop::Shutdown;
            }
        }
    }
}

/// Result returned by a backend execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome<R> {
    /// The backend returned its typed result normally.
    Completed {
        /// Backend-defined success or failure result.
        output: R,
        /// Whether execution crossed an effect boundary.
        effect: CommandEffect,
    },
    /// A bounded identity-bearing primitive completed and its output must be
    /// retained even when a cooperative stop raced its final reply.
    ///
    /// This is reserved for operations such as managed process launch and
    /// termination where discarding the returned identity would orphan an
    /// already-observed external effect. Generic cancellable work should use
    /// [`Self::Completed`] or [`Self::Stopped`].
    AtomicCompleted {
        /// Backend-defined immutable identity-bearing result.
        output: R,
        /// Whether execution crossed an effect boundary.
        effect: CommandEffect,
    },
    /// The backend cooperatively stopped and reports its effect boundary.
    Stopped {
        /// Whether execution crossed an effect boundary before stopping.
        effect: CommandEffect,
    },
}

/// Stable terminal cause retained by the command ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCause {
    /// The backend returned a typed result.
    Returned,
    /// Explicit cancellation was honored.
    Cancelled,
    /// The monotonic deadline was honored.
    DeadlineExceeded,
    /// Execution belonged to a retired desktop generation.
    GenerationChanged,
    /// Execution stopped during coordinator shutdown.
    Shutdown,
    /// The backend stopped without a coordinator stop request.
    UnexpectedStop,
    /// The backend future panicked at the supervised boundary.
    ExecutorPanicked,
}

/// Immutable terminal result retained for duplicate submissions and watchers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTerminal<R> {
    /// Why execution became terminal.
    pub cause: TerminalCause,
    /// Whether an externally visible effect occurred.
    pub effect: CommandEffect,
    /// Backend-defined output, present only after an ordinary return.
    pub output: Option<R>,
}

impl<R> CommandTerminal<R> {
    fn returned(output: R, effect: CommandEffect) -> Self {
        Self {
            cause: TerminalCause::Returned,
            effect,
            output: Some(output),
        }
    }

    const fn stopped(cause: TerminalCause, effect: CommandEffect) -> Self {
        Self {
            cause,
            effect,
            output: None,
        }
    }
}

/// Fenced request for conservative backend-owned input cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetRequest {
    /// Monotonically increasing process-local reset attempt identifier.
    pub reset_epoch: u64,
    /// Lease transaction being cleaned up.
    pub lease: LeaseGrant,
    /// Why admission was revoked.
    pub reason: RevocationReason,
    /// Current authoritative generation when the attempt began.
    pub current_generation: GenerationToken,
}

/// Backend reset completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// Conservative cleanup completed successfully.
    Complete,
    /// Cleanup could not be proved complete; the lease remains fenced.
    Failed,
}

/// Typed seam between coordinator policy and effectful backends.
pub trait CommandExecutor<C, R>: Send + Sync + 'static {
    /// Executes one uniquely admitted command.
    fn execute(
        &self,
        command: C,
        context: ExecutionContext,
    ) -> BoxCoordinatorFuture<ExecutionOutcome<R>>;

    /// Attempts conservative cleanup for one lease-reset transaction.
    fn reset_owned_input(&self, request: ResetRequest) -> BoxCoordinatorFuture<ResetOutcome>;
}

/// Actor sizing and state-machine policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoordinatorSettings {
    desktop_id: DesktopId,
    initial_generation: DesktopGeneration,
    mailbox_capacity: usize,
    maximum_active_commands: usize,
    maximum_active_per_principal: usize,
    maximum_concurrent_executions: usize,
    lease_policy: LeasePolicy,
    ledger_limits: CommandLedgerLimits,
    event_limits: EventHubLimits,
}

impl CoordinatorSettings {
    /// Creates settings with explicit finite bounds.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        desktop_id: DesktopId,
        initial_generation: DesktopGeneration,
        mailbox_capacity: usize,
        maximum_active_commands: usize,
        maximum_active_per_principal: usize,
        maximum_concurrent_executions: usize,
        lease_policy: LeasePolicy,
        ledger_limits: CommandLedgerLimits,
        event_limits: EventHubLimits,
    ) -> Result<Self, CoordinatorError> {
        if mailbox_capacity == 0
            || maximum_active_commands == 0
            || maximum_active_per_principal == 0
            || maximum_concurrent_executions == 0
            || maximum_active_per_principal > maximum_active_commands
            || maximum_concurrent_executions > maximum_active_commands
        {
            return Err(CoordinatorError::InvalidSettings);
        }
        Ok(Self {
            desktop_id,
            initial_generation,
            mailbox_capacity,
            maximum_active_commands,
            maximum_active_per_principal,
            maximum_concurrent_executions,
            lease_policy,
            ledger_limits,
            event_limits,
        })
    }

    /// Returns the bounded actor mailbox capacity.
    #[must_use]
    pub const fn mailbox_capacity(self) -> usize {
        self.mailbox_capacity
    }

    /// Returns the maximum number of concurrently running backend commands.
    #[must_use]
    pub const fn maximum_concurrent_executions(self) -> usize {
        self.maximum_concurrent_executions
    }

    /// Returns the accepted/running global command ceiling.
    #[must_use]
    pub const fn maximum_active_commands(self) -> usize {
        self.maximum_active_commands
    }

    /// Returns the accepted/running ceiling for one principal.
    #[must_use]
    pub const fn maximum_active_per_principal(self) -> usize {
        self.maximum_active_per_principal
    }
}

/// Result of submitting a deduplicated command.
#[derive(Debug)]
pub struct CommandSubmission<R: Clone> {
    /// Whether this caller atomically won execution admission.
    pub admitted: bool,
    /// Lifecycle snapshot captured atomically with the admission/dedupe reply.
    pub record: CommandRecord<CommandTerminal<R>>,
    /// Accepted, running, and immutable terminal lifecycle updates.
    pub updates: watch::Receiver<CommandRecord<CommandTerminal<R>>>,
}

/// Result of an explicit cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelCommandOutcome {
    /// A queued command was completed or a running command was signalled.
    Accepted,
    /// The command was already terminal and remains unchanged.
    AlreadyTerminal,
    /// No command exists in the authenticated principal scope.
    NotFound,
}

/// Result of asking the actor to retry a previously failed reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetRetryOutcome {
    /// A new fenced reset attempt started.
    Started,
    /// A reset attempt is already running.
    AlreadyRunning,
    /// The lease transaction is not waiting for reset completion.
    NotRequired,
}

/// Physical-input lease proof evaluated atomically with command admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRequirement {
    /// The command does not mutate global physical input.
    NotRequired,
    /// The command requires this exact controller-lease capability.
    Required(ControlLeaseId),
}

/// Coordinator admission, state-machine, or lifecycle failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CoordinatorError {
    /// A finite actor bound was zero.
    #[error("coordinator mailbox and execution bounds must be non-zero")]
    InvalidSettings,
    /// The accepted/running global command ceiling is full.
    #[error("global active-command capacity is exhausted")]
    CommandCapacityExhausted,
    /// One principal reached its accepted/running command ceiling.
    #[error("principal active-command capacity is exhausted")]
    PrincipalCommandCapacityExhausted,
    /// The actor no longer accepts messages.
    #[error("coordinator is closed")]
    Closed,
    /// A relative command deadline could not be represented.
    #[error("command deadline overflowed monotonic time")]
    DeadlineOverflow,
    /// Desktop-generation fencing failed.
    #[error(transparent)]
    Generation(#[from] GenerationFenceError),
    /// Lease state transition failed.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// Command-ledger transition failed.
    #[error(transparent)]
    Ledger(#[from] CommandLedgerError),
    /// Event replay state transition failed.
    #[error(transparent)]
    Event(#[from] EventHubError),
}

/// Cloneable async interface to the single-owner coordinator task.
#[derive(Debug)]
pub struct CoordinatorHandle<C, R: Clone, E> {
    sender: mpsc::Sender<Message<C, R, E>>,
}

impl<C, R: Clone, E> Clone for CoordinatorHandle<C, R, E> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<C, R, E> CoordinatorHandle<C, R, E>
where
    C: Send + 'static,
    R: Clone + Send + Sync + 'static,
    E: Clone + Send + 'static,
{
    /// Returns the current authoritative generation token.
    pub async fn generation(&self) -> Result<GenerationToken, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::Generation { reply }).await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    /// Returns an immutable lease transaction snapshot.
    pub async fn lease_snapshot(&self) -> Result<LeaseSnapshot, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::LeaseSnapshot { reply }).await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    /// Acquires exclusive physical-input control in the supplied generation.
    pub async fn acquire_lease(
        &self,
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        ttl_ms: Option<u64>,
        generation: GenerationToken,
    ) -> Result<LeaseGrant, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::AcquireLease {
            owner,
            lease_id,
            ttl_ms,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Renews an unexpired, exactly matching lease capability.
    pub async fn renew_lease(
        &self,
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        ttl_ms: Option<u64>,
        generation: GenerationToken,
    ) -> Result<LeaseGrant, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::RenewLease {
            owner,
            lease_id,
            ttl_ms,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Releases a matching lease and starts mandatory conservative reset.
    pub async fn release_lease(
        &self,
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        generation: GenerationToken,
    ) -> Result<(), CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::ReleaseLease {
            owner,
            lease_id,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Revokes an active lease for a coordinator-owned reason.
    pub async fn revoke_lease(&self, reason: RevocationReason) -> Result<bool, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::RevokeLease { reason, reply }).await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    /// Retries cleanup after a prior backend reset failure.
    pub async fn retry_reset(&self) -> Result<ResetRetryOutcome, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::RetryReset { reply }).await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Atomically admits or deduplicates a command.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit(
        &self,
        principal: PrincipalId,
        command_id: CommandId,
        request_hash: CanonicalCommandHash,
        generation: GenerationToken,
        lease: LeaseRequirement,
        execute_within: Option<Duration>,
        command: C,
    ) -> Result<CommandSubmission<R>, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::Submit {
            principal,
            command_id,
            request_hash,
            generation,
            lease,
            execute_within,
            command,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Looks up a retained command in the authenticated principal scope.
    pub async fn lookup_command(
        &self,
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
    ) -> Result<Option<CommandRecord<CommandTerminal<R>>>, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::Lookup {
            principal,
            command_id,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Subscribes to one retained principal-scoped command lifecycle.
    ///
    /// Terminal records yield an immediately terminal receiver. Active records
    /// share the actor-owned watch channel, avoiding polling loops in long-poll
    /// and WebSocket adapters.
    pub async fn watch_command(
        &self,
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
    ) -> Result<Option<watch::Receiver<CommandRecord<CommandTerminal<R>>>>, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::WatchLookup {
            principal,
            command_id,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Requests explicit cooperative cancellation.
    pub async fn cancel_command(
        &self,
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
    ) -> Result<CancelCommandOutcome, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::Cancel {
            principal,
            command_id,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Publishes one globally sequenced normalized event.
    pub async fn publish_event(
        &self,
        event: E,
        encoded_size: usize,
        generation: GenerationToken,
    ) -> Result<super::PublishOutcome, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::PublishEvent {
            event,
            encoded_size,
            generation,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Replays a complete retained event suffix or requests resynchronization.
    pub async fn replay_events(
        &self,
        generation: GenerationToken,
        since_sequence: u64,
    ) -> Result<ReplayResult<E>, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::ReplayEvents {
            generation,
            since_sequence,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    /// Atomically installs a bounded live receiver and snapshots retained replay.
    ///
    /// This is the only safe reconnect/subscription primitive: composing
    /// [`Self::replay_events`] with a later live subscription would leave a
    /// publication race between the two operations.
    pub async fn subscribe_events(
        &self,
        generation: GenerationToken,
        since_sequence: Option<u64>,
    ) -> Result<EventSubscription<E>, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::SubscribeEvents {
            generation,
            since_sequence,
            reply,
        })
        .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    /// Rotates every generation-fenced state machine atomically in actor order.
    pub async fn rotate_generation(
        &self,
        generation: DesktopGeneration,
    ) -> Result<GenerationToken, CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::RotateGeneration { generation, reply })
            .await?;
        receive.await.map_err(|_| CoordinatorError::Closed)?
    }

    /// Starts graceful shutdown and waits for running work and reset attempts.
    pub async fn shutdown(&self) -> Result<(), CoordinatorError> {
        let (reply, receive) = oneshot::channel();
        self.send(Message::Shutdown { reply }).await?;
        receive.await.map_err(|_| CoordinatorError::Closed)
    }

    async fn send(&self, message: Message<C, R, E>) -> Result<(), CoordinatorError> {
        self.sender
            .send(message)
            .await
            .map_err(|_| CoordinatorError::Closed)
    }
}

/// Starts one coordinator task and returns its bounded handle and join handle.
pub fn spawn_coordinator<C, R, E, B>(
    settings: CoordinatorSettings,
    backend: B,
) -> Result<SpawnedCoordinator<C, R, E>, CoordinatorError>
where
    C: Send + 'static,
    R: Clone + Send + Sync + 'static,
    E: Clone + Send + 'static,
    B: CommandExecutor<C, R>,
{
    spawn_coordinator_with_event_mapper(settings, backend, NoCommandEvents)
}

/// Starts a coordinator with one central command-lifecycle event mapper.
pub fn spawn_coordinator_with_event_mapper<C, R, E, B, M>(
    settings: CoordinatorSettings,
    backend: B,
    event_mapper: M,
) -> Result<SpawnedCoordinator<C, R, E>, CoordinatorError>
where
    C: Send + 'static,
    R: Clone + Send + Sync + 'static,
    E: Clone + Send + 'static,
    B: CommandExecutor<C, R>,
    M: CommandEventMapper<R, E>,
{
    let generation_fence = GenerationFence::new(settings.desktop_id, settings.initial_generation);
    let generation = generation_fence.capture();
    let lease = LeaseMachine::new(settings.desktop_id, generation, settings.lease_policy)?;
    let ledger = CommandLedger::new(settings.desktop_id, generation, settings.ledger_limits)?;
    let events = EventHub::new(settings.desktop_id, generation, settings.event_limits)?;
    let (live_events, _unused_live_receiver) = broadcast::channel(LIVE_EVENT_FANOUT_CAPACITY);
    let (sender, receiver) = mpsc::channel(settings.mailbox_capacity);
    let handle = CoordinatorHandle { sender };
    let task = CoordinatorTask {
        settings,
        backend: Arc::new(backend),
        event_mapper: Arc::new(event_mapper),
        receiver,
        generation_fence,
        lease,
        ledger,
        events,
        live_events,
        origin: Instant::now(),
        active: BTreeMap::new(),
        pending: VecDeque::new(),
        running: 0,
        tasks: JoinSet::new(),
        next_reset_epoch: 1,
        active_reset: None,
        reset_failed: false,
        shutting_down: false,
        shutdown_reply: None,
    };
    Ok((handle, tokio::spawn(task.run())))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ActiveKey {
    generation: GenerationToken,
    principal: PrincipalId,
    command_id: CommandId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePhase {
    Pending,
    Running,
}

#[derive(Debug)]
struct ActiveCommand<C, R: Clone> {
    command: Option<C>,
    deadline: Option<Instant>,
    phase: ActivePhase,
    stop: watch::Sender<ExecutionStop>,
    updates: watch::Sender<CommandRecord<CommandTerminal<R>>>,
    record: CommandRecord<CommandTerminal<R>>,
}

struct CommandAdmission<C> {
    principal: PrincipalId,
    command_id: CommandId,
    request_hash: CanonicalCommandHash,
    generation: GenerationToken,
    lease: LeaseRequirement,
    execute_within: Option<Duration>,
    command: C,
}

enum Message<C, R: Clone, E> {
    Generation {
        reply: oneshot::Sender<GenerationToken>,
    },
    LeaseSnapshot {
        reply: oneshot::Sender<LeaseSnapshot>,
    },
    AcquireLease {
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        ttl_ms: Option<u64>,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<LeaseGrant, CoordinatorError>>,
    },
    RenewLease {
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        ttl_ms: Option<u64>,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<LeaseGrant, CoordinatorError>>,
    },
    ReleaseLease {
        owner: PrincipalId,
        lease_id: ControlLeaseId,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<(), CoordinatorError>>,
    },
    RevokeLease {
        reason: RevocationReason,
        reply: oneshot::Sender<bool>,
    },
    RetryReset {
        reply: oneshot::Sender<Result<ResetRetryOutcome, CoordinatorError>>,
    },
    Submit {
        principal: PrincipalId,
        command_id: CommandId,
        request_hash: CanonicalCommandHash,
        generation: GenerationToken,
        lease: LeaseRequirement,
        execute_within: Option<Duration>,
        command: C,
        reply: oneshot::Sender<Result<CommandSubmission<R>, CoordinatorError>>,
    },
    Lookup {
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<Option<CommandRecord<CommandTerminal<R>>>, CoordinatorError>>,
    },
    WatchLookup {
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
        reply: CommandWatchReply<R>,
    },
    Cancel {
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<CancelCommandOutcome, CoordinatorError>>,
    },
    PublishEvent {
        event: E,
        encoded_size: usize,
        generation: GenerationToken,
        reply: oneshot::Sender<Result<super::PublishOutcome, CoordinatorError>>,
    },
    ReplayEvents {
        generation: GenerationToken,
        since_sequence: u64,
        reply: oneshot::Sender<ReplayResult<E>>,
    },
    SubscribeEvents {
        generation: GenerationToken,
        since_sequence: Option<u64>,
        reply: oneshot::Sender<EventSubscription<E>>,
    },
    RotateGeneration {
        generation: DesktopGeneration,
        reply: oneshot::Sender<Result<GenerationToken, CoordinatorError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

enum TaskCompletion<R> {
    Execution {
        key: ActiveKey,
        outcome: Result<ExecutionOutcome<R>, ()>,
    },
    Reset {
        reset_epoch: u64,
        outcome: ResetOutcome,
    },
}

struct CoordinatorTask<C, R: Clone, E, B, M> {
    settings: CoordinatorSettings,
    backend: Arc<B>,
    event_mapper: Arc<M>,
    receiver: mpsc::Receiver<Message<C, R, E>>,
    generation_fence: GenerationFence,
    lease: LeaseMachine,
    ledger: CommandLedger<CommandTerminal<R>>,
    events: EventHub<E>,
    live_events: broadcast::Sender<u64>,
    origin: Instant,
    active: BTreeMap<ActiveKey, ActiveCommand<C, R>>,
    pending: VecDeque<ActiveKey>,
    running: usize,
    tasks: JoinSet<TaskCompletion<R>>,
    next_reset_epoch: u64,
    active_reset: Option<u64>,
    reset_failed: bool,
    shutting_down: bool,
    shutdown_reply: Option<oneshot::Sender<()>>,
}

impl<C, R, E, B, M> CoordinatorTask<C, R, E, B, M>
where
    C: Send + 'static,
    R: Clone + Send + Sync + 'static,
    E: Clone + Send + 'static,
    B: CommandExecutor<C, R>,
    M: CommandEventMapper<R, E>,
{
    async fn run(mut self) {
        loop {
            if self.shutting_down && self.running == 0 && self.active_reset.is_none() {
                if let Some(reply) = self.shutdown_reply.take() {
                    let _send_result = reply.send(());
                }
                break;
            }

            let wake_at = self.next_wake_at();
            enum Selected<C, R: Clone, E> {
                Message(Option<Message<C, R, E>>),
                Completion(Option<Result<TaskCompletion<R>, tokio::task::JoinError>>),
                Timer,
            }
            let selected = tokio::select! {
                message = self.receiver.recv(), if !self.shutting_down => {
                    Selected::Message(message)
                }
                completion = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    Selected::Completion(completion)
                }
                () = wait_until(wake_at) => Selected::Timer,
            };

            match selected {
                Selected::Message(Some(message)) => self.handle_message(message),
                Selected::Message(None) => self.begin_shutdown(None),
                Selected::Completion(Some(Ok(completion))) => self.handle_completion(completion),
                Selected::Completion(Some(Err(_join_error))) => self.begin_shutdown(None),
                Selected::Completion(None) | Selected::Timer => {}
            }

            self.advance_timers();
            self.drive_pending();
        }
    }

    fn handle_message(&mut self, message: Message<C, R, E>) {
        match message {
            Message::Generation { reply } => {
                let _send_result = reply.send(self.generation_fence.capture());
            }
            Message::LeaseSnapshot { reply } => {
                let _send_result = reply.send(self.lease.snapshot());
            }
            Message::AcquireLease {
                owner,
                lease_id,
                ttl_ms,
                generation,
                reply,
            } => {
                let now = self.now_millis();
                let result = self
                    .lease
                    .acquire(owner, lease_id, ttl_ms, now, generation)
                    .map_err(CoordinatorError::from);
                self.ensure_reset();
                let _send_result = reply.send(result);
            }
            Message::RenewLease {
                owner,
                lease_id,
                ttl_ms,
                generation,
                reply,
            } => {
                let now = self.now_millis();
                let result = self
                    .lease
                    .renew(&owner, lease_id, ttl_ms, now, generation)
                    .map_err(CoordinatorError::from);
                self.ensure_reset();
                let _send_result = reply.send(result);
            }
            Message::ReleaseLease {
                owner,
                lease_id,
                generation,
                reply,
            } => {
                let now = self.now_millis();
                let result = self
                    .lease
                    .release(&owner, lease_id, now, generation)
                    .map_err(CoordinatorError::from);
                self.ensure_reset();
                let _send_result = reply.send(result);
            }
            Message::RevokeLease { reason, reply } => {
                let revoked = self.lease.revoke(reason);
                self.ensure_reset();
                let _send_result = reply.send(revoked);
            }
            Message::RetryReset { reply } => {
                let outcome = if self.active_reset.is_some() {
                    Ok(ResetRetryOutcome::AlreadyRunning)
                } else if self.lease.phase() != LeasePhase::Resetting {
                    Ok(ResetRetryOutcome::NotRequired)
                } else {
                    self.spawn_reset();
                    Ok(ResetRetryOutcome::Started)
                };
                let _send_result = reply.send(outcome);
            }
            Message::Submit {
                principal,
                command_id,
                request_hash,
                generation,
                lease,
                execute_within,
                command,
                reply,
            } => {
                let result = self.submit_command(CommandAdmission {
                    principal,
                    command_id,
                    request_hash,
                    generation,
                    lease,
                    execute_within,
                    command,
                });
                let _send_result = reply.send(result);
            }
            Message::Lookup {
                principal,
                command_id,
                generation,
                reply,
            } => {
                let now = self.now_millis();
                let result = self
                    .ledger
                    .lookup(&principal, command_id, now, generation)
                    .map_err(CoordinatorError::from);
                let _send_result = reply.send(result);
            }
            Message::WatchLookup {
                principal,
                command_id,
                generation,
                reply,
            } => {
                let result = self.watch_command(principal, command_id, generation);
                let _send_result = reply.send(result);
            }
            Message::Cancel {
                principal,
                command_id,
                generation,
                reply,
            } => {
                let result = self.cancel_command(principal, command_id, generation);
                let _send_result = reply.send(result);
            }
            Message::PublishEvent {
                event,
                encoded_size,
                generation,
                reply,
            } => {
                let result = self.publish_event(event, encoded_size, generation);
                let _send_result = reply.send(result);
            }
            Message::ReplayEvents {
                generation,
                since_sequence,
                reply,
            } => {
                let _send_result = reply.send(self.events.replay_since(generation, since_sequence));
            }
            Message::SubscribeEvents {
                generation,
                since_sequence,
                reply,
            } => {
                // Subscribe first. Actor serialization then makes replay and
                // live a gap-free handoff even if the suffix is empty.
                let live = self.live_events.subscribe();
                let replay = self.events.replay_since(
                    generation,
                    since_sequence.unwrap_or_else(|| self.events.latest_sequence()),
                );
                let _send_result = reply.send(EventSubscription { replay, live });
            }
            Message::RotateGeneration { generation, reply } => {
                let result = self.rotate_generation(generation);
                let _send_result = reply.send(result);
            }
            Message::Shutdown { reply } => self.begin_shutdown(Some(reply)),
        }
    }

    fn publish_event(
        &mut self,
        event: E,
        encoded_size: usize,
        generation: GenerationToken,
    ) -> Result<super::PublishOutcome, CoordinatorError> {
        if encoded_size > self.events.limits().maximum_encoded_bytes() {
            return Err(EventHubError::LiveEventTooLarge.into());
        }
        let outcome = self.events.publish(event, encoded_size, generation)?;
        // Payloads live only in the count+byte-bounded replay hub. Fanout sends
        // tiny sequence notifications; adapters retrieve the exact retained
        // record and treat any lag/eviction as a resync boundary.
        let _receiver_count = self.live_events.send(outcome.sequence);
        Ok(outcome)
    }

    fn submit_command(
        &mut self,
        admission: CommandAdmission<C>,
    ) -> Result<CommandSubmission<R>, CoordinatorError> {
        let CommandAdmission {
            principal,
            command_id,
            request_hash,
            generation,
            lease,
            execute_within,
            command,
        } = admission;
        let now_instant = Instant::now();
        let now = self.now_millis_at(now_instant);
        let deadline = execute_within
            .map(|duration| {
                now_instant
                    .checked_add(duration)
                    .ok_or(CoordinatorError::DeadlineOverflow)
            })
            .transpose()?;
        let key = ActiveKey {
            generation,
            principal: principal.clone(),
            command_id,
        };

        // Dedupe precedes lease verification: an exact retry may retrieve its
        // already accepted outcome even after its lease expires, while a
        // changed body still conflicts. The actor is the sole ledger writer, so
        // lookup -> lease authorization -> admission is race-free.
        if let Some(record) = self
            .ledger
            .lookup(&principal, command_id, now, generation)?
        {
            if record.request_hash != request_hash {
                return Err(CommandLedgerError::CommandIdConflict.into());
            }
            let updates = if let Some(active) = self.active.get(&key) {
                active.updates.subscribe()
            } else if record.state.is_terminal() {
                let (_sender, receiver) = watch::channel(record.clone());
                receiver
            } else {
                return Err(CommandLedgerError::InvariantViolation.into());
            };
            return Ok(CommandSubmission {
                admitted: false,
                record,
                updates,
            });
        }

        if self.active.len() >= self.settings.maximum_active_commands {
            return Err(CoordinatorError::CommandCapacityExhausted);
        }
        if self
            .active
            .keys()
            .filter(|active| active.principal == principal)
            .count()
            >= self.settings.maximum_active_per_principal
        {
            return Err(CoordinatorError::PrincipalCommandCapacityExhausted);
        }

        if let LeaseRequirement::Required(lease_id) = lease {
            let authorization = self
                .lease
                .authorize(&principal, lease_id, now, generation)
                .map_err(CoordinatorError::from);
            self.ensure_reset();
            authorization?;
        }

        let decision = self
            .ledger
            .admit(principal, command_id, request_hash, now, generation)?;
        match decision {
            IdempotencyDecision::Existing(_) => Err(CommandLedgerError::InvariantViolation.into()),
            IdempotencyDecision::Admitted(record) => {
                let (updates, receiver) = watch::channel(record.clone());
                let (stop, _stop_receiver) = watch::channel(ExecutionStop::Continue);
                self.active.insert(
                    key.clone(),
                    ActiveCommand {
                        command: Some(command),
                        deadline,
                        phase: ActivePhase::Pending,
                        stop,
                        updates,
                        record: record.clone(),
                    },
                );
                self.pending.push_back(key.clone());
                if self
                    .publish_command_transition(&key.principal, &record)
                    .is_err()
                {
                    self.begin_shutdown(None);
                }
                Ok(CommandSubmission {
                    admitted: true,
                    record,
                    updates: receiver,
                })
            }
        }
    }

    fn cancel_command(
        &mut self,
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
    ) -> Result<CancelCommandOutcome, CoordinatorError> {
        let now = self.now_millis();
        let key = ActiveKey {
            generation,
            principal: principal.clone(),
            command_id,
        };
        if let Some(active) = self.active.get(&key) {
            match active.phase {
                ActivePhase::Pending => {
                    let terminal = CommandTerminal::stopped(
                        TerminalCause::Cancelled,
                        CommandEffect::BeforeEffect,
                    );
                    self.complete_active(&key, terminal, now)?;
                }
                ActivePhase::Running => request_stop(active, ExecutionStop::Cancelled),
            }
            return Ok(CancelCommandOutcome::Accepted);
        }
        match self
            .ledger
            .lookup(&principal, command_id, now, generation)?
        {
            Some(record) if record.state.is_terminal() => Ok(CancelCommandOutcome::AlreadyTerminal),
            Some(_) => Err(CommandLedgerError::InvariantViolation.into()),
            None => Ok(CancelCommandOutcome::NotFound),
        }
    }

    fn watch_command(
        &mut self,
        principal: PrincipalId,
        command_id: CommandId,
        generation: GenerationToken,
    ) -> Result<Option<watch::Receiver<CommandRecord<CommandTerminal<R>>>>, CoordinatorError> {
        let now = self.now_millis();
        let record = self
            .ledger
            .lookup(&principal, command_id, now, generation)?;
        let Some(record) = record else {
            return Ok(None);
        };
        let key = ActiveKey {
            generation,
            principal,
            command_id,
        };
        if let Some(active) = self.active.get(&key) {
            return Ok(Some(active.updates.subscribe()));
        }
        if record.state.is_terminal() {
            let (_sender, receiver) = watch::channel(record);
            return Ok(Some(receiver));
        }
        Err(CommandLedgerError::InvariantViolation.into())
    }

    fn rotate_generation(
        &mut self,
        generation: DesktopGeneration,
    ) -> Result<GenerationToken, CoordinatorError> {
        let current = self.generation_fence.rotate(generation)?;
        self.lease.rotate_generation(current)?;
        self.ledger.rotate_generation(current)?;
        self.events.rotate_generation(current)?;

        let now = self.now_millis();
        let retired: Vec<_> = self
            .active
            .keys()
            .filter(|key| key.generation != current)
            .cloned()
            .collect();
        for key in retired {
            let Some(active) = self.active.get(&key) else {
                continue;
            };
            match active.phase {
                ActivePhase::Pending => {
                    let terminal = CommandTerminal::stopped(
                        TerminalCause::GenerationChanged,
                        CommandEffect::BeforeEffect,
                    );
                    self.complete_active(&key, terminal, now)?;
                }
                ActivePhase::Running => request_stop(active, ExecutionStop::GenerationChanged),
            }
        }
        self.ensure_reset();
        Ok(current)
    }

    fn handle_completion(&mut self, completion: TaskCompletion<R>) {
        match completion {
            TaskCompletion::Execution { key, outcome } => {
                self.running = self.running.saturating_sub(1);
                let Some(active) = self.active.get(&key) else {
                    return;
                };
                let stop = *active.stop.borrow();
                let terminal = match outcome {
                    Ok(ExecutionOutcome::AtomicCompleted { output, effect }) => {
                        CommandTerminal::returned(output, effect)
                    }
                    Ok(ExecutionOutcome::Completed { output, effect })
                        if stop == ExecutionStop::Continue =>
                    {
                        CommandTerminal::returned(output, effect)
                    }
                    Ok(ExecutionOutcome::Completed { effect, .. }) => {
                        CommandTerminal::stopped(terminal_cause(stop), effect)
                    }
                    Ok(ExecutionOutcome::Stopped { effect }) => {
                        CommandTerminal::stopped(terminal_cause(stop), effect)
                    }
                    Err(()) => CommandTerminal::stopped(
                        TerminalCause::ExecutorPanicked,
                        CommandEffect::AfterEffect,
                    ),
                };
                let now = self.now_millis();
                let _completion_result = self.complete_active(&key, terminal, now);
            }
            TaskCompletion::Reset {
                reset_epoch,
                outcome,
            } => {
                if self.active_reset != Some(reset_epoch) {
                    return;
                }
                self.active_reset = None;
                if outcome == ResetOutcome::Complete {
                    self.reset_failed = false;
                    let _finish_result = self.lease.finish_reset();
                } else {
                    self.reset_failed = true;
                }
            }
        }
    }

    fn complete_active(
        &mut self,
        key: &ActiveKey,
        terminal: CommandTerminal<R>,
        now: MonotonicMillis,
    ) -> Result<(), CoordinatorError> {
        let active = self
            .active
            .get(key)
            .ok_or(CommandLedgerError::UnknownCommand)?;
        let record = if key.generation == self.generation_fence.capture() {
            self.ledger.complete(
                &key.principal,
                key.command_id,
                terminal,
                now,
                key.generation,
            )?
        } else {
            let mut record = active.record.clone();
            if record.state.is_terminal() {
                return Err(CommandLedgerError::TerminalImmutable.into());
            }
            record.state = CommandRecordState::Terminal(terminal);
            record.updated_at = now;
            record
        };
        let active = self
            .active
            .remove(key)
            .ok_or(CommandLedgerError::InvariantViolation)?;
        if active.phase == ActivePhase::Pending {
            self.pending.retain(|pending_key| pending_key != key);
        }
        let event_record = record.clone();
        let _previous = active.updates.send_replace(record);
        // A generation rotation deliberately invalidates the old event stream;
        // subscribers receive a generation resync instead of old-lifetime
        // terminal events sequenced into the new lifetime.
        if event_record.generation == self.generation_fence.capture() {
            self.publish_command_transition(&key.principal, &event_record)?;
        }
        Ok(())
    }

    fn drive_pending(&mut self) {
        while self.running < self.settings.maximum_concurrent_executions {
            let Some(key) = self.pending.pop_front() else {
                return;
            };
            let Some(active) = self.active.get(&key) else {
                continue;
            };
            if active.phase != ActivePhase::Pending {
                continue;
            }
            if active
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                let terminal = CommandTerminal::stopped(
                    TerminalCause::DeadlineExceeded,
                    CommandEffect::BeforeEffect,
                );
                let now = self.now_millis();
                let _completion_result = self.complete_active(&key, terminal, now);
                continue;
            }

            let now = self.now_millis();
            let running_record =
                match self
                    .ledger
                    .mark_running(&key.principal, key.command_id, now, key.generation)
                {
                    Ok(record) => record,
                    Err(_error) => {
                        // A pending key and its ledger record are one actor-owned
                        // transaction. If that invariant is ever violated, close
                        // admission and terminalize everything we still own rather
                        // than silently dropping the popped queue entry forever.
                        self.begin_shutdown(None);
                        return;
                    }
                };
            let Some(active) = self.active.get_mut(&key) else {
                self.begin_shutdown(None);
                return;
            };
            let Some(command) = active.command.take() else {
                self.begin_shutdown(None);
                return;
            };
            active.phase = ActivePhase::Running;
            active.record = running_record.clone();
            let event_record = running_record.clone();
            let _previous = active.updates.send_replace(running_record);
            let context = ExecutionContext {
                generation: key.generation,
                deadline: active.deadline,
                stop: active.stop.subscribe(),
            };
            let backend = Arc::clone(&self.backend);
            let completion_key = key.clone();
            self.tasks.spawn(async move {
                let outcome = AssertUnwindSafe(backend.execute(command, context))
                    .catch_unwind()
                    .await
                    .map_err(|_panic| ());
                TaskCompletion::Execution {
                    key: completion_key,
                    outcome,
                }
            });
            self.running += 1;
            if self
                .publish_command_transition(&key.principal, &event_record)
                .is_err()
            {
                self.begin_shutdown(None);
                return;
            }
        }
    }

    fn publish_command_transition(
        &mut self,
        principal: &PrincipalId,
        record: &CommandRecord<CommandTerminal<R>>,
    ) -> Result<(), CoordinatorError> {
        let events = self.event_mapper.map_command_transition(principal, record);
        for normalized in events {
            self.publish_event(normalized.event, normalized.encoded_size, record.generation)?;
        }
        Ok(())
    }

    fn advance_timers(&mut self) {
        let now_instant = Instant::now();
        let now = self.now_millis_at(now_instant);
        let _expiry_result = self.lease.advance_time(now);
        self.ensure_reset();

        let due: Vec<_> = self
            .active
            .iter()
            .filter(|(_, active)| {
                active
                    .deadline
                    .is_some_and(|deadline| deadline <= now_instant)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            let Some(active) = self.active.get(&key) else {
                continue;
            };
            match active.phase {
                ActivePhase::Pending => {
                    let terminal = CommandTerminal::stopped(
                        TerminalCause::DeadlineExceeded,
                        CommandEffect::BeforeEffect,
                    );
                    let _completion_result = self.complete_active(&key, terminal, now);
                }
                ActivePhase::Running => request_stop(active, ExecutionStop::DeadlineExceeded),
            }
        }
    }

    fn ensure_reset(&mut self) {
        if self.lease.phase() == LeasePhase::Revoking {
            if self.lease.begin_reset().is_err() {
                return;
            }
            self.reset_failed = false;
        }
        if self.lease.phase() == LeasePhase::Resetting
            && self.active_reset.is_none()
            && !self.reset_failed
        {
            self.spawn_reset();
        }
    }

    fn spawn_reset(&mut self) {
        let LeaseSnapshot::Resetting { lease, reason } = self.lease.snapshot() else {
            return;
        };
        let reset_epoch = self.next_reset_epoch;
        let Some(next) = reset_epoch.checked_add(1) else {
            self.reset_failed = true;
            return;
        };
        self.next_reset_epoch = next;
        self.active_reset = Some(reset_epoch);
        self.reset_failed = false;
        let backend = Arc::clone(&self.backend);
        let request = ResetRequest {
            reset_epoch,
            lease,
            reason,
            current_generation: self.generation_fence.capture(),
        };
        self.tasks.spawn(async move {
            let outcome = AssertUnwindSafe(backend.reset_owned_input(request))
                .catch_unwind()
                .await
                .unwrap_or(ResetOutcome::Failed);
            TaskCompletion::Reset {
                reset_epoch,
                outcome,
            }
        });
    }

    fn begin_shutdown(&mut self, reply: Option<oneshot::Sender<()>>) {
        if self.shutting_down {
            if let Some(reply) = reply {
                let _send_result = reply.send(());
            }
            return;
        }
        self.shutting_down = true;
        self.shutdown_reply = reply;
        self.receiver.close();
        self.lease.revoke(RevocationReason::Shutdown);
        self.reset_failed = false;
        self.ensure_reset();

        let now = self.now_millis();
        let keys: Vec<_> = self.active.keys().cloned().collect();
        for key in keys {
            let Some(active) = self.active.get(&key) else {
                continue;
            };
            match active.phase {
                ActivePhase::Pending => {
                    let terminal = CommandTerminal::stopped(
                        TerminalCause::Shutdown,
                        CommandEffect::BeforeEffect,
                    );
                    let _completion_result = self.complete_active(&key, terminal, now);
                }
                ActivePhase::Running => request_stop(active, ExecutionStop::Shutdown),
            }
        }
    }

    fn next_wake_at(&self) -> Option<Instant> {
        let lease_deadline = match self.lease.snapshot() {
            LeaseSnapshot::Held(lease) => self
                .origin
                .checked_add(Duration::from_millis(lease.expires_at.get())),
            _ => None,
        };
        self.active
            .values()
            .filter_map(|active| active.deadline)
            .chain(lease_deadline)
            .min()
    }

    fn now_millis(&self) -> MonotonicMillis {
        self.now_millis_at(Instant::now())
    }

    fn now_millis_at(&self, instant: Instant) -> MonotonicMillis {
        let elapsed = instant.saturating_duration_since(self.origin).as_millis();
        let bounded = u64::try_from(elapsed).unwrap_or(u64::MAX);
        MonotonicMillis::new(bounded)
    }
}

fn request_stop<C, R: Clone>(active: &ActiveCommand<C, R>, reason: ExecutionStop) {
    if *active.stop.borrow() == ExecutionStop::Continue {
        let _previous = active.stop.send_replace(reason);
    }
}

const fn terminal_cause(stop: ExecutionStop) -> TerminalCause {
    match stop {
        ExecutionStop::Continue => TerminalCause::UnexpectedStop,
        ExecutionStop::Cancelled => TerminalCause::Cancelled,
        ExecutionStop::DeadlineExceeded => TerminalCause::DeadlineExceeded,
        ExecutionStop::GenerationChanged => TerminalCause::GenerationChanged,
        ExecutionStop::Shutdown => TerminalCause::Shutdown,
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::sync::Semaphore;
    use xenoteer_protocol::{LaunchId, ProcessRef};

    use super::*;

    #[derive(Clone)]
    struct TestBackend {
        executions: Arc<AtomicUsize>,
        block_execution: bool,
        reset_gate: Arc<Semaphore>,
        resets: Arc<AtomicUsize>,
    }

    impl TestBackend {
        fn immediate() -> Self {
            Self {
                executions: Arc::new(AtomicUsize::new(0)),
                block_execution: false,
                reset_gate: Arc::new(Semaphore::new(32)),
                resets: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn blocked() -> Self {
            Self {
                executions: Arc::new(AtomicUsize::new(0)),
                block_execution: true,
                reset_gate: Arc::new(Semaphore::new(0)),
                resets: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl CommandExecutor<u8, u8> for TestBackend {
        fn execute(
            &self,
            command: u8,
            mut context: ExecutionContext,
        ) -> BoxCoordinatorFuture<ExecutionOutcome<u8>> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let block = self.block_execution;
            Box::pin(async move {
                if block {
                    let _reason = context.wait_for_stop().await;
                    ExecutionOutcome::Stopped {
                        effect: CommandEffect::BeforeEffect,
                    }
                } else {
                    ExecutionOutcome::Completed {
                        output: command,
                        effect: CommandEffect::AfterEffect,
                    }
                }
            })
        }

        fn reset_owned_input(&self, _request: ResetRequest) -> BoxCoordinatorFuture<ResetOutcome> {
            self.resets.fetch_add(1, Ordering::SeqCst);
            let gate = Arc::clone(&self.reset_gate);
            Box::pin(async move {
                match gate.acquire().await {
                    Ok(permit) => {
                        permit.forget();
                        ResetOutcome::Complete
                    }
                    Err(_closed) => ResetOutcome::Failed,
                }
            })
        }
    }

    #[derive(Clone)]
    struct AtomicProcessBackend {
        executions: Arc<AtomicUsize>,
        started: Arc<Semaphore>,
        stop_observed: Arc<Semaphore>,
        finish: Arc<Semaphore>,
    }

    impl AtomicProcessBackend {
        fn blocked_after_effect() -> Self {
            Self {
                executions: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Semaphore::new(0)),
                stop_observed: Arc::new(Semaphore::new(0)),
                finish: Arc::new(Semaphore::new(0)),
            }
        }
    }

    impl CommandExecutor<ProcessRef, ProcessRef> for AtomicProcessBackend {
        fn execute(
            &self,
            process: ProcessRef,
            mut context: ExecutionContext,
        ) -> BoxCoordinatorFuture<ExecutionOutcome<ProcessRef>> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            let stop_observed = Arc::clone(&self.stop_observed);
            let finish = Arc::clone(&self.finish);
            Box::pin(async move {
                let _reason = context.wait_for_stop().await;
                stop_observed.add_permits(1);
                if let Ok(permit) = finish.acquire().await {
                    permit.forget();
                }
                ExecutionOutcome::AtomicCompleted {
                    output: process,
                    effect: CommandEffect::AfterEffect,
                }
            })
        }

        fn reset_owned_input(&self, _request: ResetRequest) -> BoxCoordinatorFuture<ResetOutcome> {
            Box::pin(async { ResetOutcome::Complete })
        }
    }

    type TestHandle = CoordinatorHandle<u8, u8, u8>;

    #[derive(Clone)]
    struct RecordingEventMapper {
        transitions: Arc<Mutex<Vec<u8>>>,
    }

    impl CommandEventMapper<u8, u8> for RecordingEventMapper {
        fn map_command_transition(
            &self,
            _principal: &PrincipalId,
            record: &CommandRecord<CommandTerminal<u8>>,
        ) -> Vec<CoordinatorEvent<u8>> {
            let transition = match &record.state {
                CommandRecordState::Accepted => 1,
                CommandRecordState::Running => 2,
                CommandRecordState::Terminal(_) => 3,
            };
            self.transitions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(transition);
            vec![CoordinatorEvent {
                event: transition,
                encoded_size: 1,
            }]
        }
    }

    fn setup(
        backend: TestBackend,
        ttl_ms: u64,
    ) -> Result<(TestHandle, JoinHandle<()>), CoordinatorError> {
        let settings = CoordinatorSettings::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            64,
            64,
            32,
            2,
            LeasePolicy::new(ttl_ms, ttl_ms)?,
            CommandLedgerLimits::new(64, 60_000)?,
            EventHubLimits::new(64, 64 * 1024)?,
        )?;
        spawn_coordinator(settings, backend)
    }

    fn settings() -> Result<CoordinatorSettings, CoordinatorError> {
        CoordinatorSettings::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            64,
            64,
            32,
            2,
            LeasePolicy::new(1_000, 1_000)?,
            CommandLedgerLimits::new(64, 60_000)?,
            EventHubLimits::new(64, 64 * 1024)?,
        )
    }

    #[tokio::test]
    async fn atomic_event_subscription_has_no_replay_live_gap_or_duplicate()
    -> Result<(), Box<dyn std::error::Error>> {
        let (handle, join): (TestHandle, _) =
            spawn_coordinator(settings()?, TestBackend::immediate())?;
        let generation = handle.generation().await?;
        let mut subscription = handle.subscribe_events(generation, Some(0)).await?;
        assert!(matches!(
            subscription.replay,
            ReplayResult::Events {
                latest_sequence: 0,
                ref events,
                ..
            } if events.is_empty()
        ));

        let published = handle.publish_event(7, 1, generation).await?;
        assert_eq!(published.sequence, 1);
        assert_eq!(subscription.live.recv().await?, 1);
        let replay = handle.replay_events(generation, 0).await?;
        assert!(matches!(
            replay,
            ReplayResult::Events { events, .. }
                if events.iter().map(|record| record.sequence).collect::<Vec<_>>() == vec![1]
        ));
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn sequence_fanout_is_payload_constant_and_flood_lag_is_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let (handle, join): (TestHandle, _) =
            spawn_coordinator(settings()?, TestBackend::immediate())?;
        let generation = handle.generation().await?;
        let mut subscription = handle.subscribe_events(generation, None).await?;
        for event in 1_u8..=6 {
            handle.publish_event(event, 64 * 1024, generation).await?;
        }
        assert!(matches!(
            subscription.live.recv().await,
            Err(broadcast::error::RecvError::Lagged(skipped)) if skipped >= 1
        ));
        assert!(matches!(
            handle.publish_event(9, 64 * 1024 + 1, generation).await,
            Err(CoordinatorError::Event(EventHubError::LiveEventTooLarge))
        ));
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn command_lifecycle_mapper_runs_once_per_transition_not_per_duplicate()
    -> Result<(), Box<dyn std::error::Error>> {
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let mapper = RecordingEventMapper {
            transitions: Arc::clone(&transitions),
        };
        let (handle, join): (TestHandle, _) =
            spawn_coordinator_with_event_mapper(settings()?, TestBackend::immediate(), mapper)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("mapper-owner")?;
        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([91; 32]);
        let mut admitted = handle
            .submit(
                principal.clone(),
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                None,
                4,
            )
            .await?;
        let _terminal = terminal(&mut admitted.updates).await;
        let duplicate = handle
            .submit(
                principal,
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                None,
                4,
            )
            .await?;
        assert!(!duplicate.admitted);
        {
            let recorded = transitions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(recorded.as_slice(), &[1, 2, 3]);
        }
        let replay = handle.replay_events(generation, 0).await?;
        assert!(matches!(
            replay,
            ReplayResult::Events { events, .. }
                if events.iter().map(|record| record.event).collect::<Vec<_>>() == vec![1, 2, 3]
        ));
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    async fn terminal(
        updates: &mut watch::Receiver<CommandRecord<CommandTerminal<u8>>>,
    ) -> CommandRecord<CommandTerminal<u8>> {
        loop {
            let record = updates.borrow().clone();
            if record.state.is_terminal() {
                return record;
            }
            let changed = updates.changed().await;
            assert!(changed.is_ok());
        }
    }

    async fn process_terminal(
        updates: &mut watch::Receiver<CommandRecord<CommandTerminal<ProcessRef>>>,
    ) -> CommandRecord<CommandTerminal<ProcessRef>> {
        loop {
            let record = updates.borrow().clone();
            if record.state.is_terminal() {
                return record;
            }
            let changed = updates.changed().await;
            assert!(changed.is_ok());
        }
    }

    #[tokio::test]
    async fn atomic_process_success_wins_cancel_race_and_exact_retry_keeps_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = AtomicProcessBackend::blocked_after_effect();
        let executions = Arc::clone(&backend.executions);
        let started = Arc::clone(&backend.started);
        let stop_observed = Arc::clone(&backend.stop_observed);
        let finish = Arc::clone(&backend.finish);
        let (handle, join): (CoordinatorHandle<ProcessRef, ProcessRef, u8>, _) =
            spawn_coordinator(settings()?, backend)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("process-owner")?;
        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([71; 32]);
        let process = ProcessRef {
            desktop_generation: generation.generation(),
            pid: 4_242,
            proc_start_ticks: 99,
            launch_id: LaunchId::new(),
        };
        let mut admitted = handle
            .submit(
                principal.clone(),
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                None,
                process,
            )
            .await?;
        started.acquire().await?.forget();
        assert_eq!(
            handle
                .cancel_command(principal.clone(), command_id, generation)
                .await?,
            CancelCommandOutcome::Accepted
        );
        stop_observed.acquire().await?.forget();
        finish.add_permits(1);

        let terminal = process_terminal(&mut admitted.updates).await;
        assert!(matches!(
            &terminal.state,
            CommandRecordState::Terminal(CommandTerminal {
                cause: TerminalCause::Returned,
                effect: CommandEffect::AfterEffect,
                output: Some(returned),
            }) if *returned == process
        ));
        let duplicate = handle
            .submit(
                principal,
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                None,
                process,
            )
            .await?;
        assert!(!duplicate.admitted);
        assert_eq!(duplicate.record.state, terminal.state);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn atomic_process_success_wins_deadline_race_and_keeps_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = AtomicProcessBackend::blocked_after_effect();
        let executions = Arc::clone(&backend.executions);
        let started = Arc::clone(&backend.started);
        let stop_observed = Arc::clone(&backend.stop_observed);
        let finish = Arc::clone(&backend.finish);
        let (handle, join): (CoordinatorHandle<ProcessRef, ProcessRef, u8>, _) =
            spawn_coordinator(settings()?, backend)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("deadline-process-owner")?;
        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([72; 32]);
        let process = ProcessRef {
            desktop_generation: generation.generation(),
            pid: 4_243,
            proc_start_ticks: 100,
            launch_id: LaunchId::new(),
        };
        let mut admitted = handle
            .submit(
                principal.clone(),
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                Some(Duration::from_millis(10)),
                process,
            )
            .await?;
        started.acquire().await?.forget();
        tokio::time::advance(Duration::from_millis(11)).await;
        stop_observed.acquire().await?.forget();
        finish.add_permits(1);

        let terminal = process_terminal(&mut admitted.updates).await;
        assert!(matches!(
            &terminal.state,
            CommandRecordState::Terminal(CommandTerminal {
                cause: TerminalCause::Returned,
                effect: CommandEffect::AfterEffect,
                output: Some(returned),
            }) if *returned == process
        ));
        let duplicate = handle
            .submit(
                principal,
                command_id,
                hash,
                generation,
                LeaseRequirement::NotRequired,
                None,
                process,
            )
            .await?;
        assert!(!duplicate.admitted);
        assert_eq!(duplicate.record.state, terminal.state);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_duplicates_execute_exactly_once() -> Result<(), Box<dyn std::error::Error>>
    {
        let backend = TestBackend::immediate();
        let execution_count = Arc::clone(&backend.executions);
        let (handle, join) = setup(backend, 1_000)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("alice")?;
        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([7; 32]);
        let mut callers = JoinSet::new();
        for _ in 0..32 {
            let handle = handle.clone();
            let principal = principal.clone();
            callers.spawn(async move {
                handle
                    .submit(
                        principal,
                        command_id,
                        hash,
                        generation,
                        LeaseRequirement::NotRequired,
                        None,
                        9,
                    )
                    .await
            });
        }

        let mut admissions = 0;
        while let Some(call) = callers.join_next().await {
            let mut submission = call??;
            admissions += usize::from(submission.admitted);
            let result = terminal(&mut submission.updates).await;
            assert!(matches!(result.state, CommandRecordState::Terminal(_)));
        }
        assert_eq!(admissions, 1);
        assert_eq!(execution_count.load(Ordering::SeqCst), 1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn changed_duplicate_body_conflicts() -> Result<(), Box<dyn std::error::Error>> {
        let (handle, join) = setup(TestBackend::immediate(), 1_000)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("alice")?;
        let command_id = CommandId::new();
        handle
            .submit(
                principal.clone(),
                command_id,
                CanonicalCommandHash::new([1; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                1,
            )
            .await?;
        let conflict = handle
            .submit(
                principal,
                command_id,
                CanonicalCommandHash::new([2; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                1,
            )
            .await;
        assert!(matches!(
            conflict,
            Err(CoordinatorError::Ledger(
                CommandLedgerError::CommandIdConflict
            ))
        ));
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn lease_proof_is_atomic_but_exact_retry_precedes_reauthorization()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = TestBackend::immediate();
        let execution_count = Arc::clone(&backend.executions);
        let (handle, join) = setup(backend, 1_000)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("alice")?;
        let lease_id = ControlLeaseId::new();
        handle
            .acquire_lease(principal.clone(), lease_id, None, generation)
            .await?;

        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([4; 32]);
        assert!(matches!(
            handle
                .submit(
                    principal.clone(),
                    command_id,
                    hash,
                    generation,
                    LeaseRequirement::Required(ControlLeaseId::new()),
                    None,
                    4,
                )
                .await,
            Err(CoordinatorError::Lease(LeaseError::WrongLease))
        ));
        assert_eq!(execution_count.load(Ordering::SeqCst), 0);

        let mut admitted = handle
            .submit(
                principal.clone(),
                command_id,
                hash,
                generation,
                LeaseRequirement::Required(lease_id),
                None,
                4,
            )
            .await?;
        let _terminal = terminal(&mut admitted.updates).await;
        handle
            .release_lease(principal.clone(), lease_id, generation)
            .await?;

        let duplicate = handle
            .submit(
                principal,
                command_id,
                hash,
                generation,
                LeaseRequirement::Required(lease_id),
                None,
                4,
            )
            .await?;
        assert!(!duplicate.admitted);
        assert!(duplicate.updates.borrow().state.is_terminal());
        assert_eq!(execution_count.load(Ordering::SeqCst), 1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn active_capacity_is_bounded_globally_and_per_principal()
    -> Result<(), Box<dyn std::error::Error>> {
        let settings = CoordinatorSettings::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            16,
            2,
            1,
            1,
            LeasePolicy::default(),
            CommandLedgerLimits::new(16, 60_000)?,
            EventHubLimits::new(16, 16 * 1024)?,
        )?;
        let (handle, join): (TestHandle, _) = spawn_coordinator(settings, TestBackend::blocked())?;
        let generation = handle.generation().await?;
        let alice = PrincipalId::new("alice")?;
        let bob = PrincipalId::new("bob")?;

        let _first = handle
            .submit(
                alice.clone(),
                CommandId::new(),
                CanonicalCommandHash::new([1; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                1,
            )
            .await?;
        assert!(matches!(
            handle
                .submit(
                    alice,
                    CommandId::new(),
                    CanonicalCommandHash::new([2; 32]),
                    generation,
                    LeaseRequirement::NotRequired,
                    None,
                    2,
                )
                .await,
            Err(CoordinatorError::PrincipalCommandCapacityExhausted)
        ));
        let _second = handle
            .submit(
                bob,
                CommandId::new(),
                CanonicalCommandHash::new([3; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                3,
            )
            .await?;
        assert!(matches!(
            handle
                .submit(
                    PrincipalId::new("charlie")?,
                    CommandId::new(),
                    CanonicalCommandHash::new([4; 32]),
                    generation,
                    LeaseRequirement::NotRequired,
                    None,
                    4,
                )
                .await,
            Err(CoordinatorError::CommandCapacityExhausted)
        ));

        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_subscription_observes_terminal_without_polling()
    -> Result<(), Box<dyn std::error::Error>> {
        let (handle, join) = setup(TestBackend::blocked(), 1_000)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("alice")?;
        let command_id = CommandId::new();
        let _submission = handle
            .submit(
                principal.clone(),
                command_id,
                CanonicalCommandHash::new([8; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                8,
            )
            .await?;
        let mut updates = handle
            .watch_command(principal.clone(), command_id, generation)
            .await?
            .ok_or_else(|| std::io::Error::other("missing active subscription"))?;
        assert_eq!(
            handle
                .cancel_command(principal.clone(), command_id, generation)
                .await?,
            CancelCommandOutcome::Accepted
        );
        let terminal_record = terminal(&mut updates).await;
        assert!(terminal_record.state.is_terminal());
        let retained = handle
            .watch_command(principal, command_id, generation)
            .await?
            .ok_or_else(|| std::io::Error::other("missing retained subscription"))?;
        assert!(retained.borrow().state.is_terminal());
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_blocks_reacquisition_until_fenced_reset_finishes()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = TestBackend::blocked();
        let reset_gate = Arc::clone(&backend.reset_gate);
        let resets = Arc::clone(&backend.resets);
        let (handle, join) = setup(backend, 20)?;
        let generation = handle.generation().await?;
        let owner = PrincipalId::new("alice")?;
        handle
            .acquire_lease(owner, ControlLeaseId::new(), None, generation)
            .await?;

        tokio::time::advance(Duration::from_millis(21)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            handle.lease_snapshot().await?.phase(),
            LeasePhase::Resetting
        );
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        assert!(matches!(
            handle
                .acquire_lease(
                    PrincipalId::new("bob")?,
                    ControlLeaseId::new(),
                    None,
                    generation,
                )
                .await,
            Err(CoordinatorError::Lease(LeaseError::ResetRequired))
        ));

        reset_gate.add_permits(1);
        tokio::task::yield_now().await;
        assert_eq!(handle.lease_snapshot().await?.phase(), LeasePhase::Vacant);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn generation_change_fences_running_work_and_old_replay()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = TestBackend::blocked();
        let reset_gate = Arc::clone(&backend.reset_gate);
        let (handle, join) = setup(backend, 1_000)?;
        let old = handle.generation().await?;
        let mut submission = handle
            .submit(
                PrincipalId::new("alice")?,
                CommandId::new(),
                CanonicalCommandHash::new([3; 32]),
                old,
                LeaseRequirement::NotRequired,
                None,
                3,
            )
            .await?;
        tokio::task::yield_now().await;
        let current = handle.rotate_generation(DesktopGeneration::new()).await?;
        let record = terminal(&mut submission.updates).await;
        assert!(matches!(
            record.state,
            CommandRecordState::Terminal(CommandTerminal {
                cause: TerminalCause::GenerationChanged,
                ..
            })
        ));
        assert!(matches!(
            handle.replay_events(old, 0).await?,
            ReplayResult::ResyncRequired { .. }
        ));
        assert_ne!(old, current);
        reset_gate.add_permits(1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn queued_deadline_and_running_cancel_have_immutable_terminal_results()
    -> Result<(), Box<dyn std::error::Error>> {
        let backend = TestBackend::blocked();
        let reset_gate = Arc::clone(&backend.reset_gate);
        let settings = CoordinatorSettings::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            16,
            16,
            16,
            1,
            LeasePolicy::default(),
            CommandLedgerLimits::new(16, 60_000)?,
            EventHubLimits::new(16, 16 * 1024)?,
        )?;
        let (handle, join): (TestHandle, _) = spawn_coordinator(settings, backend)?;
        let generation = handle.generation().await?;
        let principal = PrincipalId::new("alice")?;
        let running_id = CommandId::new();
        let mut running = handle
            .submit(
                principal.clone(),
                running_id,
                CanonicalCommandHash::new([1; 32]),
                generation,
                LeaseRequirement::NotRequired,
                None,
                1,
            )
            .await?;
        let mut queued = handle
            .submit(
                principal.clone(),
                CommandId::new(),
                CanonicalCommandHash::new([2; 32]),
                generation,
                LeaseRequirement::NotRequired,
                Some(Duration::from_millis(10)),
                2,
            )
            .await?;
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;
        let queued_result = terminal(&mut queued.updates).await;
        assert!(matches!(
            queued_result.state,
            CommandRecordState::Terminal(CommandTerminal {
                cause: TerminalCause::DeadlineExceeded,
                effect: CommandEffect::BeforeEffect,
                ..
            })
        ));

        assert_eq!(
            handle
                .cancel_command(principal, running_id, generation)
                .await?,
            CancelCommandOutcome::Accepted
        );
        let running_result = terminal(&mut running.updates).await;
        assert!(matches!(
            running_result.state,
            CommandRecordState::Terminal(CommandTerminal {
                cause: TerminalCause::Cancelled,
                ..
            })
        ));
        reset_gate.add_permits(1);
        handle.shutdown().await?;
        join.await?;
        Ok(())
    }

    trait LeaseSnapshotPhase {
        fn phase(&self) -> LeasePhase;
    }

    impl LeaseSnapshotPhase for LeaseSnapshot {
        fn phase(&self) -> LeasePhase {
            match self {
                Self::Vacant { .. } => LeasePhase::Vacant,
                Self::Held(_) => LeasePhase::Held,
                Self::Revoking { .. } => LeasePhase::Revoking,
                Self::Resetting { .. } => LeasePhase::Resetting,
            }
        }
    }
}
