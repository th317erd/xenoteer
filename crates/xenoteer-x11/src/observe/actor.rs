//! Single-owner observation actor with bounded request and event lanes.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, Damage, ReportLevel};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window};
use x11rb::rust_connection::RustConnection;

use super::atoms::KnownAtoms;
use super::damage::DamageAccumulator;
use super::events::{ReconcileDecision, classify_reconcile};
use super::inventory::{InventoryWarning, RootInventory, initial_root_inventory};
use super::snapshot::{WindowSnapshotInput, query_window_snapshot_input};
use super::{PollThreadEvent, RootDamageBatch, normalize_event};
use crate::{ExtensionName, Result, X11Error, connect};

/// Default number of admitted observation requests.
pub const DEFAULT_OBSERVATION_REQUEST_CAPACITY: usize = 128;
/// Default number of reconcile signals retained for consumers.
pub const DEFAULT_OBSERVATION_EVENT_CAPACITY: usize = 256;

const MAX_CONTROL_WAITERS: usize = 64;
const MAX_EVENTS_PER_TURN: usize = 64;
const EVENT_POLL_BACKSTOP: Duration = Duration::from_millis(25);

/// Observable lifecycle state of the observation actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationActorState {
    /// Worker exists but its backend has not completed startup.
    Starting,
    /// Backend is live and ordinary requests are admitted.
    Healthy,
    /// A terminal backend fault permanently closed admission.
    Poisoned,
    /// Orderly shutdown or final handle closure completed.
    Stopped,
    /// A panic crossed an internal actor boundary.
    Panicked,
}

/// Latest actor-owned health evidence available without connection access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationActorHealth {
    /// Current actor lifecycle state.
    pub state: ObservationActorState,
    /// Successfully completed ordinary requests.
    pub completed_requests: u64,
    /// Terminal failure that most recently changed actor health.
    pub last_failure: Option<ObservationActorFailureKind>,
}

/// Stable failure categories for request/control responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationActorFailureKind {
    /// One request failed without invalidating the connection.
    RequestFailed,
    /// The X11 connection or its transport was lost.
    BackendUnavailable,
    /// Work was rejected after terminal backend poisoning.
    ActorPoisoned,
    /// Work was rejected because orderly shutdown began.
    ActorStopped,
    /// Work was rejected because the worker panicked.
    ActorPanicked,
    /// The bounded collection of coalesced shutdown waiters was full.
    ControlQueueFull,
}

/// Typed actor failure without leaking X11 reply internals.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("observation actor request failed: {kind:?}")]
pub struct ObservationActorFailure {
    /// Stable public failure category.
    pub kind: ObservationActorFailureKind,
}

impl ObservationActorFailure {
    const fn new(kind: ObservationActorFailureKind) -> Self {
        Self { kind }
    }
}

/// Immediate bounded-lane admission failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ObservationActorSubmitError {
    /// The ordinary lane has no remaining capacity.
    #[error("observation request queue is full")]
    QueueFull,
    /// The actor no longer admits ordinary requests.
    #[error("observation actor is closed")]
    Closed,
}

/// Terminal state returned by the owned join capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationActorExit {
    /// Orderly shutdown completed.
    Stopped,
    /// A terminal backend fault ended the actor.
    Poisoned,
    /// A panic ended the actor.
    Panicked,
}

/// One actor response receiver. Callers choose their own bounded wait.
#[derive(Debug)]
pub struct ObservationReply<T> {
    receiver: Receiver<std::result::Result<T, ObservationActorFailure>>,
}

impl<T> ObservationReply<T> {
    /// Receive a response with an explicit caller-selected deadline.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<std::result::Result<T, ObservationActorFailure>, RecvTimeoutError>
    {
        self.receiver.recv_timeout(timeout)
    }

    /// Attempt to receive without blocking.
    pub fn try_recv(
        &self,
    ) -> std::result::Result<std::result::Result<T, ObservationActorFailure>, TryRecvError> {
        self.receiver.try_recv()
    }
}

/// Bounded event emitted by the actor after pure reconciliation classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationActorEvent {
    /// One incremental model action inferred from an X11 event.
    Reconcile {
        /// Pure actor-independent reconciliation instruction.
        decision: ReconcileDecision,
    },
    /// Frame-coalesced advisory root-framebuffer damage.
    RootDamaged {
        /// Bounded root-coordinate dirty regions.
        damage: RootDamageBatch,
    },
    /// Incremental signals were lost or X11 requested a full rebuild.
    ResyncRequired,
    /// Backend observation failed.
    Failed {
        /// Stable failure evidence.
        failure: ObservationActorFailure,
    },
}

/// Receiver for the bounded, nonblocking reconcile signal lane.
pub struct ObservationActorEventReceiver {
    receiver: Receiver<ObservationActorEvent>,
}

impl ObservationActorEventReceiver {
    /// Receive a signal with an explicit caller-selected deadline.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<ObservationActorEvent, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    /// Attempt to receive without blocking.
    pub fn try_recv(&self) -> std::result::Result<ObservationActorEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

/// Cloneable bounded request/control handle. It never owns or exposes X11.
#[derive(Clone)]
pub struct ObservationActorHandle {
    ordinary: SyncSender<ObservationRequest>,
    control: Arc<SharedControl>,
    health: Arc<RwLock<ObservationActorHealth>>,
    accepting: Arc<AtomicBool>,
    wake: Arc<ActorWake>,
}

impl ObservationActorHandle {
    /// Attempt immediate admission of a fixed per-window snapshot query.
    pub fn try_snapshot(
        &self,
        window: Window,
    ) -> std::result::Result<ObservationReply<WindowSnapshotInput>, ObservationActorSubmitError>
    {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.try_send(ObservationRequest::Snapshot { window, reply })?;
        Ok(ObservationReply { receiver })
    }

    /// Attempt immediate admission of a bounded root inventory reconciliation.
    pub fn try_reconcile(
        &self,
    ) -> std::result::Result<ObservationReply<RootInventory>, ObservationActorSubmitError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.try_send(ObservationRequest::Reconcile { reply })?;
        Ok(ObservationReply { receiver })
    }

    /// Attempt immediate admission of a reply-producing X11 health check.
    pub fn try_health_check(
        &self,
    ) -> std::result::Result<ObservationReply<ObservationActorHealth>, ObservationActorSubmitError>
    {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.try_send(ObservationRequest::Health { reply })?;
        Ok(ObservationReply { receiver })
    }

    /// Return the latest actor-published health snapshot without touching X11.
    #[must_use]
    pub fn health(&self) -> ObservationActorHealth {
        *read_lock(&self.health)
    }

    /// Coalesce shutdown on the independent control path.
    #[must_use]
    pub fn shutdown(&self) -> ObservationReply<()> {
        self.accepting.store(false, Ordering::Release);
        self.control.enqueue_shutdown()
    }

    fn try_send(
        &self,
        request: ObservationRequest,
    ) -> std::result::Result<(), ObservationActorSubmitError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(ObservationActorSubmitError::Closed);
        }
        self.ordinary
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => ObservationActorSubmitError::QueueFull,
                TrySendError::Disconnected(_) => ObservationActorSubmitError::Closed,
            })?;
        self.wake.notify();
        Ok(())
    }
}

/// Owned join capability; dropping it requests shutdown and joins synchronously.
pub struct ObservationActorJoin {
    thread: Option<JoinHandle<ObservationActorExit>>,
    control: Arc<SharedControl>,
    accepting: Arc<AtomicBool>,
}

impl ObservationActorJoin {
    /// Request orderly shutdown and join, even when admission handles remain.
    pub fn join(mut self) -> ObservationActorExit {
        let Some(thread) = self.thread.take() else {
            return ObservationActorExit::Stopped;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown();
        thread.join().unwrap_or(ObservationActorExit::Panicked)
    }
}

impl Drop for ObservationActorJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown();
        let _exit = thread.join();
    }
}

/// Start the production observation actor. Display connection creation and all
/// subsequent X11 access occur on the dedicated worker thread.
pub fn spawn_observation_actor(
    display: &str,
) -> Result<(
    ObservationActorHandle,
    ObservationActorEventReceiver,
    ObservationActorJoin,
)> {
    let display = display.to_owned();
    spawn_with_backend(
        DEFAULT_OBSERVATION_REQUEST_CAPACITY,
        DEFAULT_OBSERVATION_EVENT_CAPACITY,
        move || X11ObservationBackend::open(&display),
    )
}

pub(super) fn spawn_with_backend<B, F>(
    request_capacity: usize,
    event_capacity: usize,
    factory: F,
) -> Result<(
    ObservationActorHandle,
    ObservationActorEventReceiver,
    ObservationActorJoin,
)>
where
    B: ObservationBackend,
    F: FnOnce() -> Result<B> + Send + 'static,
{
    if request_capacity == 0 || event_capacity == 0 {
        return Err(X11Error::InvalidSetup(
            "observation actor capacities must be positive",
        ));
    }
    let (ordinary_tx, ordinary_rx) = mpsc::sync_channel(request_capacity);
    let (event_tx, event_rx) = mpsc::sync_channel(event_capacity);
    let wake = Arc::new(ActorWake::default());
    let accepting = Arc::new(AtomicBool::new(true));
    let health = Arc::new(RwLock::new(ObservationActorHealth {
        state: ObservationActorState::Starting,
        completed_requests: 0,
        last_failure: None,
    }));
    let control = Arc::new(SharedControl::new(Arc::clone(&wake)));
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);

    let thread_accepting = Arc::clone(&accepting);
    let thread_health = Arc::clone(&health);
    let thread_control = Arc::clone(&control);
    let thread_wake = Arc::clone(&wake);
    let thread = thread::Builder::new()
        .name("xenoteer-observation-actor".to_owned())
        .spawn(move || {
            let backend = match catch_unwind(AssertUnwindSafe(factory)) {
                Ok(Ok(backend)) => backend,
                Ok(Err(error)) => {
                    let _ignored = startup_tx.send(Err(error));
                    return ObservationActorExit::Stopped;
                }
                Err(_) => {
                    drop(startup_tx);
                    return ObservationActorExit::Panicked;
                }
            };
            write_lock(&thread_health).state = ObservationActorState::Healthy;
            let _ignored = startup_tx.send(Ok(()));
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_actor(
                    backend,
                    ordinary_rx,
                    event_tx,
                    &thread_control,
                    &thread_health,
                    &thread_accepting,
                    &thread_wake,
                )
            }));
            match result {
                Ok(exit) => exit,
                Err(_) => {
                    thread_accepting.store(false, Ordering::Release);
                    thread_control.close(ObservationActorFailureKind::ActorPanicked);
                    let mut snapshot = write_lock(&thread_health);
                    snapshot.state = ObservationActorState::Panicked;
                    snapshot.last_failure = Some(ObservationActorFailureKind::ActorPanicked);
                    ObservationActorExit::Panicked
                }
            }
        })
        .map_err(|error| X11Error::Poll(error.to_string()))?;

    match startup_rx.recv() {
        Ok(Ok(())) => Ok((
            ObservationActorHandle {
                ordinary: ordinary_tx,
                control: Arc::clone(&control),
                health,
                accepting: Arc::clone(&accepting),
                wake,
            },
            ObservationActorEventReceiver { receiver: event_rx },
            ObservationActorJoin {
                thread: Some(thread),
                control,
                accepting,
            },
        )),
        Ok(Err(error)) => {
            let _exit = thread.join();
            Err(error)
        }
        Err(_) => {
            let _exit = thread.join();
            Err(X11Error::WorkerPanicked)
        }
    }
}

pub(super) trait ObservationBackend: Send + 'static {
    fn root(&self) -> Window;
    fn atoms(&self) -> &KnownAtoms;
    fn snapshot(
        &mut self,
        window: Window,
    ) -> std::result::Result<WindowSnapshotInput, ObservationBackendFault>;
    fn reconcile(&mut self) -> std::result::Result<RootInventory, ObservationBackendFault>;
    fn health_check(&mut self) -> std::result::Result<(), ObservationBackendFault>;
    fn poll_event(
        &mut self,
    ) -> std::result::Result<Option<PollThreadEvent>, ObservationBackendFault>;
    fn observe_window(
        &mut self,
        window: Window,
    ) -> std::result::Result<(), ObservationBackendFault>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservationBackendFault {
    kind: ObservationActorFailureKind,
    terminal: bool,
}

impl ObservationBackendFault {
    pub(super) const fn terminal(kind: ObservationActorFailureKind) -> Self {
        Self {
            kind,
            terminal: true,
        }
    }

    pub(super) const fn request_failed() -> Self {
        Self {
            kind: ObservationActorFailureKind::RequestFailed,
            terminal: false,
        }
    }
}

struct X11ObservationBackend {
    connection: RustConnection,
    root: Window,
    atoms: KnownAtoms,
    root_damage: Option<Damage>,
}

impl X11ObservationBackend {
    fn open(display: &str) -> Result<Self> {
        let opened = connect(display)?;
        let root = opened.info.root;
        let atoms = KnownAtoms::intern(&opened.connection)?;
        let root_damage = if opened
            .info
            .extensions
            .get(ExtensionName::Damage)
            .is_some_and(|extension| extension.present)
        {
            opened
                .connection
                .damage_query_version(1, 1)
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(error.to_string()))?;
            let damage = opened
                .connection
                .generate_id()
                .map_err(|error| X11Error::Connection(error.to_string()))?;
            opened
                .connection
                .damage_create(damage, root, ReportLevel::NON_EMPTY)
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .check()
                .map_err(|error| X11Error::Reply(error.to_string()))?;
            Some(damage)
        } else {
            None
        };
        opened
            .connection
            .change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(
                    EventMask::SUBSTRUCTURE_NOTIFY
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::PROPERTY_CHANGE
                        | EventMask::FOCUS_CHANGE,
                ),
            )
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .check()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        opened
            .connection
            .flush()
            .map_err(|error| X11Error::Connection(error.to_string()))?;
        Ok(Self {
            connection: opened.connection,
            root,
            atoms,
            root_damage,
        })
    }

    fn select_window_events(
        &self,
        window: Window,
    ) -> std::result::Result<(), ObservationBackendFault> {
        self.select_window_events_unchecked(window)?
            .check()
            .map_err(|error| {
                ObservationBackendFault::from_x11(crate::error::classify_reply_error(error))
            })
    }

    fn select_window_events_unchecked(
        &self,
        window: Window,
    ) -> std::result::Result<VoidCookie<'_, RustConnection>, ObservationBackendFault> {
        self.connection
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(
                    EventMask::STRUCTURE_NOTIFY
                        | EventMask::PROPERTY_CHANGE
                        | EventMask::FOCUS_CHANGE,
                ),
            )
            .map_err(|_| {
                ObservationBackendFault::terminal(ObservationActorFailureKind::BackendUnavailable)
            })
    }
}

impl ObservationBackend for X11ObservationBackend {
    fn root(&self) -> Window {
        self.root
    }

    fn atoms(&self) -> &KnownAtoms {
        &self.atoms
    }

    fn snapshot(
        &mut self,
        window: Window,
    ) -> std::result::Result<WindowSnapshotInput, ObservationBackendFault> {
        let snapshot =
            query_window_snapshot_input(&self.connection, self.root, window, &self.atoms)
                .map_err(ObservationBackendFault::from_x11)?;
        self.select_window_events(window)?;
        Ok(snapshot)
    }

    fn reconcile(&mut self) -> std::result::Result<RootInventory, ObservationBackendFault> {
        let mut inventory = initial_root_inventory(&self.connection, self.root, &self.atoms)
            .map_err(ObservationBackendFault::from_x11)?;
        // Keep every cookie alive until all subscriptions have been issued so
        // the checked round trips are pipelined rather than serialized.
        let mut cookies = Vec::with_capacity(inventory.windows.len());
        for window in inventory.windows.iter().copied() {
            cookies.push(self.select_window_events_unchecked(window)?);
        }
        let candidate_windows = std::mem::take(&mut inventory.windows);
        for (window, cookie) in candidate_windows.into_iter().zip(cookies) {
            match cookie.check() {
                Ok(()) => inventory.windows.push(window),
                Err(error) => {
                    let fault = ObservationBackendFault::from_x11(
                        crate::error::classify_reply_error(error),
                    );
                    if fault.terminal {
                        return Err(fault);
                    }
                    if !inventory
                        .warnings
                        .contains(&InventoryWarning::VanishedMember)
                    {
                        inventory.warnings.push(InventoryWarning::VanishedMember);
                    }
                }
            }
        }
        Ok(inventory)
    }

    fn health_check(&mut self) -> std::result::Result<(), ObservationBackendFault> {
        self.connection
            .get_input_focus()
            .map_err(|_| {
                ObservationBackendFault::terminal(ObservationActorFailureKind::BackendUnavailable)
            })?
            .reply()
            .map(|_| ())
            .map_err(|error| {
                ObservationBackendFault::from_x11(crate::error::classify_reply_error(error))
            })
    }

    fn poll_event(
        &mut self,
    ) -> std::result::Result<Option<PollThreadEvent>, ObservationBackendFault> {
        let event = self.connection.poll_for_event().map_err(|_| {
            ObservationBackendFault::terminal(ObservationActorFailureKind::BackendUnavailable)
        })?;
        let Some(event) = event else {
            return Ok(None);
        };
        if let Event::DamageNotify(notification) = event
            && Some(notification.damage) == self.root_damage
        {
            self.connection
                .damage_subtract(notification.damage, 0_u32, 0_u32)
                .map_err(|_| {
                    ObservationBackendFault::terminal(
                        ObservationActorFailureKind::BackendUnavailable,
                    )
                })?;
            self.connection.flush().map_err(|_| {
                ObservationBackendFault::terminal(ObservationActorFailureKind::BackendUnavailable)
            })?;
            return Ok(Some(normalize_event(Event::DamageNotify(notification))));
        }
        Ok(Some(normalize_event(event)))
    }

    fn observe_window(
        &mut self,
        window: Window,
    ) -> std::result::Result<(), ObservationBackendFault> {
        self.select_window_events(window)
    }
}

impl ObservationBackendFault {
    fn from_x11(error: X11Error) -> Self {
        match error {
            X11Error::Connect(_) | X11Error::Connection(_) | X11Error::Poll(_) => {
                Self::terminal(ObservationActorFailureKind::BackendUnavailable)
            }
            X11Error::WorkerPanicked => Self::terminal(ObservationActorFailureKind::ActorPanicked),
            _ => Self::request_failed(),
        }
    }
}

enum ObservationRequest {
    Snapshot {
        window: Window,
        reply: SyncSender<std::result::Result<WindowSnapshotInput, ObservationActorFailure>>,
    },
    Reconcile {
        reply: SyncSender<std::result::Result<RootInventory, ObservationActorFailure>>,
    },
    Health {
        reply: SyncSender<std::result::Result<ObservationActorHealth, ObservationActorFailure>>,
    },
}

impl ObservationRequest {
    fn fail(self, kind: ObservationActorFailureKind) {
        match self {
            Self::Snapshot { reply, .. } => {
                let _ignored = reply.send(Err(ObservationActorFailure::new(kind)));
            }
            Self::Reconcile { reply } => {
                let _ignored = reply.send(Err(ObservationActorFailure::new(kind)));
            }
            Self::Health { reply } => {
                let _ignored = reply.send(Err(ObservationActorFailure::new(kind)));
            }
        }
    }
}

enum RequestFlow {
    Continue,
    Poison(ObservationActorFailure),
}

fn run_actor<B: ObservationBackend>(
    mut backend: B,
    ordinary: Receiver<ObservationRequest>,
    event_sender: SyncSender<ObservationActorEvent>,
    control: &SharedControl,
    health: &RwLock<ObservationActorHealth>,
    accepting: &AtomicBool,
    wake: &ActorWake,
) -> ObservationActorExit {
    let mut emitter = ActorEventEmitter::new(event_sender);
    let mut damage = DamageAccumulator::default();
    let mut wake_sequence = 0;
    loop {
        if let Some(waiters) = control.take_shutdown() {
            accepting.store(false, Ordering::Release);
            reject_queued(&ordinary, ObservationActorFailureKind::ActorStopped);
            control.complete_shutdown(waiters);
            write_lock(health).state = ObservationActorState::Stopped;
            return ObservationActorExit::Stopped;
        }

        let mut did_work = false;
        for _ in 0..MAX_EVENTS_PER_TURN {
            match backend.poll_event() {
                Ok(Some(event)) => {
                    did_work = true;
                    let event = match event {
                        PollThreadEvent::RootDamage { hint } => {
                            damage.offer(hint, Instant::now());
                            continue;
                        }
                        event => event,
                    };
                    match process_event(&mut backend, event, &mut emitter) {
                        RequestFlow::Continue => {}
                        RequestFlow::Poison(failure) => {
                            return poison(
                                &ordinary,
                                &mut emitter,
                                control,
                                health,
                                accepting,
                                failure,
                            );
                        }
                    }
                }
                Ok(None) => break,
                Err(fault) if fault.terminal => {
                    return poison(
                        &ordinary,
                        &mut emitter,
                        control,
                        health,
                        accepting,
                        ObservationActorFailure::new(fault.kind),
                    );
                }
                Err(fault) => {
                    did_work = true;
                    emitter.offer(ObservationActorEvent::Failed {
                        failure: ObservationActorFailure::new(fault.kind),
                    });
                    break;
                }
            }
        }
        if let Some(batch) = damage.take_due(Instant::now()) {
            emitter.offer(ObservationActorEvent::RootDamaged { damage: batch });
        }
        emitter.flush_resync();

        match ordinary.try_recv() {
            Ok(request) => {
                did_work = true;
                if let RequestFlow::Poison(failure) = process_request(&mut backend, request, health)
                {
                    return poison(&ordinary, &mut emitter, control, health, accepting, failure);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                accepting.store(false, Ordering::Release);
                control.close(ObservationActorFailureKind::ActorStopped);
                write_lock(health).state = ObservationActorState::Stopped;
                return ObservationActorExit::Stopped;
            }
        }

        if !did_work {
            wake_sequence = wake.wait(
                wake_sequence,
                damage.wait_timeout(Instant::now(), EVENT_POLL_BACKSTOP),
            );
        }
    }
}

fn process_event<B: ObservationBackend>(
    backend: &mut B,
    event: PollThreadEvent,
    emitter: &mut ActorEventEmitter,
) -> RequestFlow {
    if event == PollThreadEvent::ResyncRequired {
        emitter.offer(ObservationActorEvent::ResyncRequired);
        return RequestFlow::Continue;
    }
    let decision = classify_reconcile(&event, backend.root(), backend.atoms());
    if let ReconcileDecision::ObserveWindow { window } = decision
        && let Err(fault) = backend.observe_window(window)
    {
        let failure = ObservationActorFailure::new(fault.kind);
        if fault.terminal {
            return RequestFlow::Poison(failure);
        }
        // Without a checked per-window event subscription, later incremental
        // evidence for this XID is incomplete. Make the gap explicit and ask
        // the model owner to rebuild instead of emitting ObserveWindow.
        emitter.offer(ObservationActorEvent::Failed { failure });
        emitter.offer(ObservationActorEvent::ResyncRequired);
        return RequestFlow::Continue;
    }
    match decision {
        ReconcileDecision::Ignore => {}
        ReconcileDecision::FullResync => {
            emitter.offer(ObservationActorEvent::Reconcile { decision });
        }
        ReconcileDecision::ConnectionFailed => {
            return RequestFlow::Poison(ObservationActorFailure::new(
                ObservationActorFailureKind::BackendUnavailable,
            ));
        }
        decision => emitter.offer(ObservationActorEvent::Reconcile { decision }),
    }
    RequestFlow::Continue
}

fn process_request<B: ObservationBackend>(
    backend: &mut B,
    request: ObservationRequest,
    health: &RwLock<ObservationActorHealth>,
) -> RequestFlow {
    match request {
        ObservationRequest::Snapshot { window, reply } => {
            let result = backend.snapshot(window);
            finish_request(result, reply, health)
        }
        ObservationRequest::Reconcile { reply } => {
            let result = backend.reconcile();
            finish_request(result, reply, health)
        }
        ObservationRequest::Health { reply } => match backend.health_check() {
            Ok(()) => {
                let snapshot = complete_request(health);
                let _ignored = reply.send(Ok(snapshot));
                RequestFlow::Continue
            }
            Err(fault) => finish_fault(fault, reply, health),
        },
    }
}

fn finish_request<T>(
    result: std::result::Result<T, ObservationBackendFault>,
    reply: SyncSender<std::result::Result<T, ObservationActorFailure>>,
    health: &RwLock<ObservationActorHealth>,
) -> RequestFlow {
    match result {
        Ok(value) => {
            complete_request(health);
            let _ignored = reply.send(Ok(value));
            RequestFlow::Continue
        }
        Err(fault) => finish_fault(fault, reply, health),
    }
}

fn finish_fault<T>(
    fault: ObservationBackendFault,
    reply: SyncSender<std::result::Result<T, ObservationActorFailure>>,
    health: &RwLock<ObservationActorHealth>,
) -> RequestFlow {
    let failure = ObservationActorFailure::new(fault.kind);
    if fault.terminal {
        let mut snapshot = write_lock(health);
        snapshot.state = ObservationActorState::Poisoned;
        snapshot.last_failure = Some(failure.kind);
    }
    let _ignored = reply.send(Err(failure));
    if fault.terminal {
        RequestFlow::Poison(failure)
    } else {
        RequestFlow::Continue
    }
}

fn complete_request(health: &RwLock<ObservationActorHealth>) -> ObservationActorHealth {
    let mut snapshot = write_lock(health);
    snapshot.completed_requests = snapshot.completed_requests.saturating_add(1);
    *snapshot
}

fn poison(
    ordinary: &Receiver<ObservationRequest>,
    emitter: &mut ActorEventEmitter,
    control: &SharedControl,
    health: &RwLock<ObservationActorHealth>,
    accepting: &AtomicBool,
    failure: ObservationActorFailure,
) -> ObservationActorExit {
    accepting.store(false, Ordering::Release);
    {
        let mut snapshot = write_lock(health);
        snapshot.state = ObservationActorState::Poisoned;
        snapshot.last_failure = Some(failure.kind);
    }
    reject_queued(ordinary, ObservationActorFailureKind::ActorPoisoned);
    emitter.offer(ObservationActorEvent::Failed { failure });
    control.close(ObservationActorFailureKind::ActorPoisoned);
    ObservationActorExit::Poisoned
}

fn reject_queued(ordinary: &Receiver<ObservationRequest>, kind: ObservationActorFailureKind) {
    while let Ok(request) = ordinary.try_recv() {
        request.fail(kind);
    }
}

struct ActorEventEmitter {
    sender: SyncSender<ObservationActorEvent>,
    resync_latched: bool,
    last_root_region: Option<super::RootDamageRect>,
    full_damage_latched: bool,
}

impl ActorEventEmitter {
    const fn new(sender: SyncSender<ObservationActorEvent>) -> Self {
        Self {
            sender,
            resync_latched: false,
            last_root_region: None,
            full_damage_latched: false,
        }
    }

    fn offer(&mut self, event: ObservationActorEvent) {
        if let ObservationActorEvent::RootDamaged { damage } = &event {
            self.last_root_region = Some(damage.root_region);
        }
        self.flush_resync();
        if self.resync_latched || self.full_damage_latched {
            // Once a root region has been observed, overflow recovery is an
            // ordered two-record barrier: ResyncRequired followed by a full
            // root-damage marker. Later incremental events are already covered
            // by that barrier and must not relatch another resync between the
            // two records when the bounded channel admits only one at a time.
            if self.resync_latched {
                self.latch_resync();
            }
            return;
        }
        match self.sender.try_send(event) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => self.latch_resync(),
        }
    }

    fn flush_resync(&mut self) {
        if self.resync_latched {
            match self.sender.try_send(ObservationActorEvent::ResyncRequired) {
                Ok(()) => self.resync_latched = false,
                Err(TrySendError::Disconnected(_)) => {
                    self.resync_latched = false;
                    self.full_damage_latched = false;
                    return;
                }
                Err(TrySendError::Full(_)) => return,
            }
        }
        if !self.full_damage_latched {
            return;
        }
        let Some(root_region) = self.last_root_region else {
            self.full_damage_latched = false;
            return;
        };
        let marker = ObservationActorEvent::RootDamaged {
            damage: RootDamageBatch {
                root_region,
                regions: vec![root_region],
                coverage: super::RootDamageCoverage::FullScreen,
                notifications: 1,
            },
        };
        match self.sender.try_send(marker) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => self.full_damage_latched = false,
            Err(TrySendError::Full(_)) => {}
        }
    }

    fn latch_resync(&mut self) {
        self.resync_latched = true;
        if self.last_root_region.is_some() {
            self.full_damage_latched = true;
        }
    }
}

#[derive(Default)]
struct ActorWake {
    sequence: Mutex<u64>,
    condvar: Condvar,
}

impl ActorWake {
    fn notify(&self) {
        let mut sequence = lock_mutex(&self.sequence);
        *sequence = sequence.wrapping_add(1);
        self.condvar.notify_one();
    }

    fn wait(&self, observed: u64, timeout: Duration) -> u64 {
        let sequence = lock_mutex(&self.sequence);
        if *sequence != observed {
            return *sequence;
        }
        let (sequence, _timeout) = self
            .condvar
            .wait_timeout(sequence, timeout)
            .unwrap_or_else(|error| error.into_inner());
        *sequence
    }
}

struct PendingControl {
    shutdown: bool,
    waiters: Vec<SyncSender<std::result::Result<(), ObservationActorFailure>>>,
    closed: Option<ObservationActorFailureKind>,
}

struct SharedControl {
    pending: Mutex<PendingControl>,
    wake: Arc<ActorWake>,
}

impl SharedControl {
    fn new(wake: Arc<ActorWake>) -> Self {
        Self {
            pending: Mutex::new(PendingControl {
                shutdown: false,
                waiters: Vec::new(),
                closed: None,
            }),
            wake,
        }
    }

    fn enqueue_shutdown(&self) -> ObservationReply<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        {
            let mut pending = lock_mutex(&self.pending);
            if let Some(kind) = pending.closed {
                let _ignored = reply.send(Err(ObservationActorFailure::new(kind)));
                return ObservationReply { receiver };
            }
            if pending.waiters.len() == MAX_CONTROL_WAITERS {
                let _ignored = reply.send(Err(ObservationActorFailure::new(
                    ObservationActorFailureKind::ControlQueueFull,
                )));
                return ObservationReply { receiver };
            }
            pending.shutdown = true;
            pending.waiters.push(reply);
        }
        self.wake.notify();
        ObservationReply { receiver }
    }

    fn take_shutdown(
        &self,
    ) -> Option<Vec<SyncSender<std::result::Result<(), ObservationActorFailure>>>> {
        let mut pending = lock_mutex(&self.pending);
        if !pending.shutdown {
            return None;
        }
        pending.shutdown = false;
        Some(std::mem::take(&mut pending.waiters))
    }

    fn close(&self, kind: ObservationActorFailureKind) {
        let mut pending = lock_mutex(&self.pending);
        if pending.closed.is_some() {
            return;
        }
        pending.closed = Some(kind);
        pending.shutdown = false;
        let waiters = std::mem::take(&mut pending.waiters);
        drop(pending);
        for waiter in waiters {
            let _ignored = waiter.send(Err(ObservationActorFailure::new(kind)));
        }
    }

    fn complete_shutdown(
        &self,
        mut waiters: Vec<SyncSender<std::result::Result<(), ObservationActorFailure>>>,
    ) {
        let mut pending = lock_mutex(&self.pending);
        if pending.closed.is_none() {
            pending.closed = Some(ObservationActorFailureKind::ActorStopped);
            pending.shutdown = false;
            waiters.append(&mut pending.waiters);
        }
        drop(pending);
        for waiter in waiters {
            let _ignored = waiter.send(Ok(()));
        }
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
