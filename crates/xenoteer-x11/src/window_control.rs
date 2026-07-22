//! Bounded, single-owner raw X11 window-control adapter.
//!
//! This layer deliberately accepts XIDs rather than protocol `WindowRef`s. It
//! neither authorizes commands nor asserts that an XID still names the same
//! client. Every admitted operation therefore requires a caller-provided
//! revalidator, which runs on the owner thread immediately before the first
//! X11 request made for that operation. The returned evidence is advisory raw
//! X11 evidence; the daemon remains responsible for binding it to identity and
//! model generations.
//!
//! Control owns a connection distinct from observation. Bounded convergence
//! polling can delay only this control queue and cannot starve event draining
//! or model resynchronization on the observation connection.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use x11rb::protocol::xproto::Window;
use xenoteer_protocol::{
    WindowCloseWaitPolicy, WindowControlWarning, WindowGeometry, WindowGeometryRequest,
    WindowGeometryTarget, WindowManagerCapability, WindowManagerState, WindowRect,
    WindowScreenBoundsPolicy, WindowStackMode,
};

use crate::FocusAncestryStatus;
use crate::{Result, X11Error};

mod x11;

use x11::X11WindowControlBackend;

/// Default number of admitted raw window-control operations.
pub const DEFAULT_WINDOW_CONTROL_REQUEST_CAPACITY: usize = 64;
/// Default upper bound applied by [`RawWindowControlRequest::validate`].
pub const MAX_WINDOW_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Production convergence polling interval.
pub const WINDOW_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(20);

const CONTROL_BACKSTOP: Duration = Duration::from_millis(10);
const MAX_SHUTDOWN_WAITERS: usize = 64;

/// One already-authorized raw XID operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWindowControlRequest {
    /// Target client XID. Zero is never admitted.
    pub target: Window,
    /// Cooperative window-manager operation.
    pub operation: RawWindowControlOperation,
    /// Bounded post-effect convergence budget.
    pub timeout: Duration,
}

impl RawWindowControlRequest {
    /// Validate raw shape and the hard deadline ceiling.
    pub fn validate(&self) -> std::result::Result<(), RawWindowControlRequestError> {
        if self.target == 0 {
            return Err(RawWindowControlRequestError::InvalidTarget);
        }
        if self.timeout.is_zero() || self.timeout > MAX_WINDOW_CONTROL_TIMEOUT {
            return Err(RawWindowControlRequestError::InvalidTimeout);
        }
        match self.operation {
            RawWindowControlOperation::MoveResize { geometry, .. } => geometry
                .validate()
                .map_err(|_| RawWindowControlRequestError::InvalidGeometry),
            RawWindowControlOperation::Stack {
                mode,
                sibling,
                allow_raw_fallback: _,
            } => match (mode, sibling) {
                (WindowStackMode::Above | WindowStackMode::Below, Some(sibling))
                    if sibling != 0 && sibling != self.target =>
                {
                    Ok(())
                }
                (WindowStackMode::Raise | WindowStackMode::Lower, None) => Ok(()),
                _ => Err(RawWindowControlRequestError::InvalidSibling),
            },
            RawWindowControlOperation::MoveToWorkspace { workspace } if workspace == u32::MAX => {
                Err(RawWindowControlRequestError::InvalidWorkspace)
            }
            RawWindowControlOperation::Activate {
                switch_workspace: Some(workspace),
                ..
            } if workspace == u32::MAX => Err(RawWindowControlRequestError::InvalidWorkspace),
            _ => Ok(()),
        }
    }
}

/// Desired raw operation. Focus fallback is explicit; no variant can kill a process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawWindowControlOperation {
    /// Send `_NET_ACTIVE_WINDOW`; zero timestamp explicitly means CurrentTime.
    Activate {
        /// Timestamp derived from the initiating input event when available.
        timestamp: u32,
        /// Optional zero-based workspace to switch to before activation.
        switch_workspace: Option<u32>,
        /// Permit the documented checked core focus fallback.
        allow_set_input_focus: bool,
    },
    /// Request cooperative close, optionally awaiting unmap/destruction.
    Close {
        /// Timestamp derived from the initiating input event when available.
        timestamp: u32,
        /// Requested close postcondition.
        wait_for: WindowCloseWaitPolicy,
    },
    /// Add or remove a reviewed `_NET_WM_STATE`; toggle is intentionally absent.
    SetState {
        /// State projection.
        state: WindowManagerState,
        /// Desired final value.
        desired: bool,
    },
    /// Request ICCCM iconification or EWMH activation/deiconification.
    Minimize {
        /// Desired final minimized value.
        desired: bool,
        /// Timestamp used by the activation-based restore path.
        timestamp: u32,
    },
    /// Resolve and apply root-physical geometry from live root/frame evidence.
    MoveResize {
        /// Whether requested fields describe the frame or client rectangle.
        relative_to: WindowGeometryTarget,
        /// Fields to change; unspecified fields remain untouched.
        geometry: WindowGeometryRequest,
        /// Policy applied against the live root rectangle before the EWMH request.
        bounds_policy: WindowScreenBoundsPolicy,
    },
    /// Move the target to one zero-based workspace.
    MoveToWorkspace {
        /// Desired zero-based EWMH desktop index.
        workspace: u32,
    },
    /// Best-effort EWMH restacking, with an explicit raw fallback opt-in.
    Stack {
        /// Desired relative mode.
        mode: WindowStackMode,
        /// Required only for `Above` and `Below`.
        sibling: Option<Window>,
        /// Permit a checked core `ConfigureWindow` fallback.
        allow_raw_fallback: bool,
    },
}

/// Invalid raw request rejected before queue admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RawWindowControlRequestError {
    /// XID zero is the sentinel `None`, not a client.
    #[error("window-control target must be a non-zero XID")]
    InvalidTarget,
    /// Deadline was zero or exceeded the fixed ceiling.
    #[error("window-control timeout is outside the supported range")]
    InvalidTimeout,
    /// Partial geometry was empty or contained an invalid extent.
    #[error("window-control geometry is invalid")]
    InvalidGeometry,
    /// Stack mode and sibling did not form a valid pair.
    #[error("window-control stack sibling is invalid")]
    InvalidSibling,
    /// The all-desktops sentinel must be expressed as sticky state instead.
    #[error("window-control workspace is invalid")]
    InvalidWorkspace,
}

/// Upstream exact-reference validation result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RawWindowRevalidationError {
    /// The previously resolved exact birth no longer maps to this XID.
    #[error("the exact window reference became stale before effect")]
    StaleReference,
    /// The caller rejected the operation without exposing policy internals.
    #[error("the raw window operation was rejected before effect")]
    Rejected,
}

/// Stable result status for an advisory window-manager request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawWindowControlOutcome {
    /// Observed evidence satisfied the desired state.
    Converged,
    /// The checked request crossed a same-connection barrier; no wait requested.
    RequestSent,
    /// Required EWMH/ICCCM support was not advertised.
    Unsupported,
    /// Capability or observation data was malformed/truncated and not trusted.
    MalformedWindowManagerData,
    /// Live policy normalization rejected the desired geometry before effect.
    InvalidGeometry,
    /// X11 or the window manager explicitly rejected the checked request.
    Refused,
    /// Target disappeared before or during the operation.
    TargetVanished,
    /// Deadline elapsed without convergence.
    TimedOut,
    /// Some compound or advisory evidence changed but did not fully converge.
    Partial,
}

/// Normalized three-valued state evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawWindowBooleanObservation {
    /// Desired state atoms/evidence were fully enabled.
    Enabled,
    /// Desired state atoms/evidence were fully disabled.
    Disabled,
    /// Only one member of a compound state was observed.
    Partial,
}

/// Root-physical client geometry sampled after an operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWindowGeometryObservation {
    /// Last live client/frame geometry sampled by the control owner.
    pub observed: WindowGeometry,
    /// Full normalized rectangle in the operation's requested frame/client space.
    pub effective: WindowRect,
    /// Exact static-gravity client fields sent to `_NET_MOVERESIZE_WINDOW`.
    pub client_request: WindowGeometryRequest,
    /// Whether root clamping changed the full desired rectangle.
    pub bounds_constrained: bool,
    /// Whether the observed geometry remained configure-quiet for the required window.
    pub quiet: bool,
}

/// Typed raw evidence. XIDs here are observations, not stable identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawWindowControlObservation {
    /// No trustworthy post-effect observation was available.
    NotObserved,
    /// Activation evidence from EWMH and core focus.
    Activation {
        /// Active XID before the request, also sent in the client message.
        current_active_sent: Option<Window>,
        /// Timestamp sent to the window manager.
        timestamp_sent: u32,
        /// Final `_NET_ACTIVE_WINDOW` value.
        active: Option<Window>,
        /// Final core input-focus XID.
        focused: Option<Window>,
        /// Whether core focus is the target or a proven descendant.
        focus_within_target: bool,
        /// Terminal status of bounded QueryTree focus normalization.
        focus_ancestry_status: FocusAncestryStatus,
        /// Final root `_NET_CURRENT_DESKTOP` value.
        current_workspace: Option<u32>,
    },
    /// Close target liveness and mapping evidence.
    Close {
        /// Whether the XID still existed.
        exists: bool,
        /// Whether an existing target remained viewable.
        viewable: Option<bool>,
    },
    /// Desired-state observation.
    State(RawWindowBooleanObservation),
    /// Geometry sampled in root coordinates.
    Geometry(RawWindowGeometryObservation),
    /// Final window workspace value.
    Workspace(Option<u32>),
    /// Final positions in `_NET_CLIENT_LIST_STACKING`.
    Stacking {
        /// Target index, bottom to top.
        target_index: Option<u32>,
        /// Sibling index, bottom to top.
        sibling_index: Option<u32>,
        /// Number of windows in the complete stacking list.
        window_count: u32,
    },
}

/// Operation-level capabilities decoded from a complete `_NET_SUPPORTED` list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWindowManagerCapabilities {
    /// Capabilities shared with the public protocol contract.
    pub supported: Vec<WindowManagerCapability>,
    /// Whether `_NET_RESTACK_WINDOW` is advertised.
    pub restack: bool,
}

/// Requested raw operation plus bounded observed evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWindowControlEvidence {
    /// Exact raw request presented to this adapter.
    pub requested: RawWindowControlRequest,
    /// Stable advisory outcome.
    pub outcome: RawWindowControlOutcome,
    /// Last trustworthy raw observation.
    pub observed: RawWindowControlObservation,
    /// Complete capability projection used to choose the request path.
    pub capabilities: Option<RawWindowManagerCapabilities>,
    /// Bounded protocol warnings relevant to this operation.
    pub warnings: Vec<WindowControlWarning>,
}

impl RawWindowControlEvidence {
    fn without_observation(
        requested: RawWindowControlRequest,
        outcome: RawWindowControlOutcome,
    ) -> Self {
        Self {
            requested,
            outcome,
            observed: RawWindowControlObservation::NotObserved,
            capabilities: None,
            warnings: Vec::new(),
        }
    }
}

/// Lifecycle state of the distinct raw control actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlActorState {
    /// Backend factory is still starting.
    Starting,
    /// Requests are admitted.
    Healthy,
    /// A terminal backend fault permanently closed admission.
    Poisoned,
    /// Orderly shutdown completed.
    Stopped,
    /// The owner thread panicked.
    Panicked,
}

/// Latest actor health available without X11 access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowControlActorHealth {
    /// Current lifecycle state.
    pub state: WindowControlActorState,
    /// Successfully processed operation requests and read-only capability probes.
    pub completed_requests: u64,
    /// Last terminal actor failure.
    pub last_failure: Option<WindowControlActorFailureKind>,
}

/// Stable actor/request failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlActorFailureKind {
    /// Exact-reference revalidation reported staleness.
    StaleReference,
    /// Pre-effect revalidation rejected the operation.
    RevalidationRejected,
    /// A read-only capability probe observed malformed/truncated WM data.
    MalformedWindowManagerData,
    /// A read-only capability probe was refused without losing the connection.
    CapabilityProbeFailed,
    /// X11 transport became unusable.
    BackendUnavailable,
    /// Actor was poisoned by a terminal backend fault.
    ActorPoisoned,
    /// Orderly shutdown began.
    ActorStopped,
    /// Owner thread panicked.
    ActorPanicked,
    /// Bounded shutdown waiter collection was full.
    ControlQueueFull,
}

/// Typed failure without X11 reply strings or policy details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("window-control actor request failed: {kind:?}")]
pub struct WindowControlActorFailure {
    /// Stable failure category.
    pub kind: WindowControlActorFailureKind,
}

impl WindowControlActorFailure {
    const fn new(kind: WindowControlActorFailureKind) -> Self {
        Self { kind }
    }
}

/// Immediate bounded admission error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WindowControlSubmitError {
    /// Request shape was rejected before admission.
    #[error(transparent)]
    InvalidRequest(#[from] RawWindowControlRequestError),
    /// Ordinary queue has no remaining capacity.
    #[error("window-control request queue is full")]
    QueueFull,
    /// Actor no longer admits work.
    #[error("window-control actor is closed")]
    Closed,
}

/// Owner-thread terminal state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlActorExit {
    /// Orderly shutdown.
    Stopped,
    /// Terminal backend failure.
    Poisoned,
    /// Panic contained at the actor boundary.
    Panicked,
}

/// Response receiver with caller-selected bounded waiting.
#[derive(Debug)]
pub struct WindowControlReply<T> {
    receiver: Receiver<std::result::Result<T, WindowControlActorFailure>>,
}

impl<T> WindowControlReply<T> {
    /// Receive with an explicit caller-selected timeout.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<std::result::Result<T, WindowControlActorFailure>, RecvTimeoutError>
    {
        self.receiver.recv_timeout(timeout)
    }
}

type Revalidator =
    Box<dyn FnOnce() -> std::result::Result<(), RawWindowRevalidationError> + Send + 'static>;

enum ActorRequest {
    Execute {
        request: RawWindowControlRequest,
        revalidate: Revalidator,
        reply: SyncSender<std::result::Result<RawWindowControlEvidence, WindowControlActorFailure>>,
    },
    Capabilities {
        reply: SyncSender<
            std::result::Result<RawWindowManagerCapabilities, WindowControlActorFailure>,
        >,
    },
}

/// Cloneable bounded admission/control handle. It never exposes its connection.
#[derive(Clone)]
pub struct WindowControlActorHandle {
    ordinary: SyncSender<ActorRequest>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
    health: Arc<RwLock<WindowControlActorHealth>>,
}

impl WindowControlActorHandle {
    /// Admit one request with a mandatory exact-reference revalidation hook.
    ///
    /// The hook is invoked exactly once on the connection owner thread after
    /// dequeue and immediately before the backend's first X11 operation.
    pub fn try_submit<F>(
        &self,
        request: RawWindowControlRequest,
        revalidate: F,
    ) -> std::result::Result<WindowControlReply<RawWindowControlEvidence>, WindowControlSubmitError>
    where
        F: FnOnce() -> std::result::Result<(), RawWindowRevalidationError> + Send + 'static,
    {
        request.validate()?;
        if !self.accepting.load(Ordering::Acquire) {
            return Err(WindowControlSubmitError::Closed);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.ordinary
            .try_send(ActorRequest::Execute {
                request,
                revalidate: Box::new(revalidate),
                reply,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => WindowControlSubmitError::QueueFull,
                TrySendError::Disconnected(_) => WindowControlSubmitError::Closed,
            })?;
        Ok(WindowControlReply { receiver })
    }

    /// Admit one bounded, read-only snapshot of the complete `_NET_SUPPORTED` projection.
    ///
    /// Probes run on the same owner connection and queue as effects, so callers
    /// receive identical backpressure, shutdown, and terminal-health semantics.
    pub fn try_capabilities(
        &self,
    ) -> std::result::Result<
        WindowControlReply<RawWindowManagerCapabilities>,
        WindowControlSubmitError,
    > {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(WindowControlSubmitError::Closed);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.ordinary
            .try_send(ActorRequest::Capabilities { reply })
            .map_err(|error| match error {
                TrySendError::Full(_) => WindowControlSubmitError::QueueFull,
                TrySendError::Disconnected(_) => WindowControlSubmitError::Closed,
            })?;
        Ok(WindowControlReply { receiver })
    }

    /// Return actor health without touching X11.
    #[must_use]
    pub fn health(&self) -> WindowControlActorHealth {
        *read_lock(&self.health)
    }

    /// Coalesce orderly shutdown on the independent control path.
    #[must_use]
    pub fn shutdown(&self) -> WindowControlReply<()> {
        self.accepting.store(false, Ordering::Release);
        self.control.enqueue_shutdown()
    }
}

/// Owned join capability. Joining initiates orderly shutdown.
pub struct WindowControlActorJoin {
    thread: Option<JoinHandle<WindowControlActorExit>>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
}

impl WindowControlActorJoin {
    /// Request shutdown and join the owner thread.
    pub fn join(mut self) -> WindowControlActorExit {
        let Some(thread) = self.thread.take() else {
            return WindowControlActorExit::Stopped;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown();
        thread.join().unwrap_or(WindowControlActorExit::Panicked)
    }
}

impl Drop for WindowControlActorJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown();
        let _exit = thread.join();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendFault {
    Unsupported,
    Refused,
    TargetVanished,
    MalformedWindowManagerData,
    BackendUnavailable,
}

trait WindowControlBackend: Send + 'static {
    fn capabilities(&mut self) -> std::result::Result<RawWindowManagerCapabilities, BackendFault>;

    fn execute(
        &mut self,
        request: &RawWindowControlRequest,
    ) -> std::result::Result<RawWindowControlEvidence, BackendFault>;
}

/// Spawn the production actor and create its distinct X11 connection on the
/// owner thread.
pub fn spawn_window_control_actor(
    display: &str,
) -> Result<(WindowControlActorHandle, WindowControlActorJoin)> {
    let display = display.to_owned();
    spawn_with_backend(DEFAULT_WINDOW_CONTROL_REQUEST_CAPACITY, move || {
        X11WindowControlBackend::open(&display)
    })
}

fn spawn_with_backend<B, F>(
    request_capacity: usize,
    factory: F,
) -> Result<(WindowControlActorHandle, WindowControlActorJoin)>
where
    B: WindowControlBackend,
    F: FnOnce() -> Result<B> + Send + 'static,
{
    if request_capacity == 0 {
        return Err(X11Error::InvalidSetup(
            "window-control actor capacity must be positive",
        ));
    }
    let (ordinary_tx, ordinary_rx) = mpsc::sync_channel(request_capacity);
    let accepting = Arc::new(AtomicBool::new(true));
    let health = Arc::new(RwLock::new(WindowControlActorHealth {
        state: WindowControlActorState::Starting,
        completed_requests: 0,
        last_failure: None,
    }));
    let control = Arc::new(ActorControl::default());
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);

    let thread_accepting = Arc::clone(&accepting);
    let thread_health = Arc::clone(&health);
    let thread_control = Arc::clone(&control);
    let thread = thread::Builder::new()
        .name("xenoteer-window-control-actor".to_owned())
        .spawn(move || {
            let backend = match catch_unwind(AssertUnwindSafe(factory)) {
                Ok(Ok(backend)) => backend,
                Ok(Err(error)) => {
                    let _ignored = startup_tx.send(Err(error));
                    return WindowControlActorExit::Stopped;
                }
                Err(_) => {
                    drop(startup_tx);
                    return WindowControlActorExit::Panicked;
                }
            };
            write_lock(&thread_health).state = WindowControlActorState::Healthy;
            let _ignored = startup_tx.send(Ok(()));
            match catch_unwind(AssertUnwindSafe(|| {
                run_actor(
                    backend,
                    ordinary_rx,
                    &thread_control,
                    &thread_health,
                    &thread_accepting,
                )
            })) {
                Ok(exit) => exit,
                Err(_) => {
                    thread_accepting.store(false, Ordering::Release);
                    thread_control.close(WindowControlActorFailureKind::ActorPanicked);
                    let mut health = write_lock(&thread_health);
                    health.state = WindowControlActorState::Panicked;
                    health.last_failure = Some(WindowControlActorFailureKind::ActorPanicked);
                    WindowControlActorExit::Panicked
                }
            }
        })
        .map_err(|error| X11Error::Poll(error.to_string()))?;

    match startup_rx.recv() {
        Ok(Ok(())) => Ok((
            WindowControlActorHandle {
                ordinary: ordinary_tx,
                control: Arc::clone(&control),
                accepting: Arc::clone(&accepting),
                health,
            },
            WindowControlActorJoin {
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

fn run_actor<B: WindowControlBackend>(
    mut backend: B,
    ordinary: Receiver<ActorRequest>,
    control: &ActorControl,
    health: &RwLock<WindowControlActorHealth>,
    accepting: &AtomicBool,
) -> WindowControlActorExit {
    loop {
        if let Some(waiters) = control.take_shutdown() {
            accepting.store(false, Ordering::Release);
            reject_queued(&ordinary, WindowControlActorFailureKind::ActorStopped);
            control.complete_shutdown(waiters);
            write_lock(health).state = WindowControlActorState::Stopped;
            return WindowControlActorExit::Stopped;
        }
        let request = match ordinary.recv_timeout(CONTROL_BACKSTOP) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                accepting.store(false, Ordering::Release);
                control.close(WindowControlActorFailureKind::ActorStopped);
                write_lock(health).state = WindowControlActorState::Stopped;
                return WindowControlActorExit::Stopped;
            }
        };

        let (request, revalidate, reply) = match request {
            ActorRequest::Execute {
                request,
                revalidate,
                reply,
            } => (request, revalidate, reply),
            ActorRequest::Capabilities { reply } => match backend.capabilities() {
                Ok(capabilities) => {
                    let mut snapshot = write_lock(health);
                    snapshot.completed_requests = snapshot.completed_requests.saturating_add(1);
                    drop(snapshot);
                    let _ignored = reply.send(Ok(capabilities));
                    continue;
                }
                Err(BackendFault::BackendUnavailable) => {
                    let failure = WindowControlActorFailure::new(
                        WindowControlActorFailureKind::BackendUnavailable,
                    );
                    let _ignored = reply.send(Err(failure));
                    accepting.store(false, Ordering::Release);
                    reject_queued(&ordinary, WindowControlActorFailureKind::ActorPoisoned);
                    control.close(WindowControlActorFailureKind::ActorPoisoned);
                    let mut health = write_lock(health);
                    health.state = WindowControlActorState::Poisoned;
                    health.last_failure = Some(failure.kind);
                    return WindowControlActorExit::Poisoned;
                }
                Err(BackendFault::MalformedWindowManagerData) => {
                    let _ignored = reply.send(Err(WindowControlActorFailure::new(
                        WindowControlActorFailureKind::MalformedWindowManagerData,
                    )));
                    continue;
                }
                Err(
                    BackendFault::Unsupported
                    | BackendFault::Refused
                    | BackendFault::TargetVanished,
                ) => {
                    let _ignored = reply.send(Err(WindowControlActorFailure::new(
                        WindowControlActorFailureKind::CapabilityProbeFailed,
                    )));
                    continue;
                }
            },
        };
        if let Err(error) = revalidate() {
            let kind = match error {
                RawWindowRevalidationError::StaleReference => {
                    WindowControlActorFailureKind::StaleReference
                }
                RawWindowRevalidationError::Rejected => {
                    WindowControlActorFailureKind::RevalidationRejected
                }
            };
            let _ignored = reply.send(Err(WindowControlActorFailure::new(kind)));
            continue;
        }

        match backend.execute(&request) {
            Ok(evidence) => {
                let mut snapshot = write_lock(health);
                snapshot.completed_requests = snapshot.completed_requests.saturating_add(1);
                drop(snapshot);
                let _ignored = reply.send(Ok(evidence));
            }
            Err(BackendFault::BackendUnavailable) => {
                let failure = WindowControlActorFailure::new(
                    WindowControlActorFailureKind::BackendUnavailable,
                );
                let _ignored = reply.send(Err(failure));
                accepting.store(false, Ordering::Release);
                reject_queued(&ordinary, WindowControlActorFailureKind::ActorPoisoned);
                control.close(WindowControlActorFailureKind::ActorPoisoned);
                let mut health = write_lock(health);
                health.state = WindowControlActorState::Poisoned;
                health.last_failure = Some(failure.kind);
                return WindowControlActorExit::Poisoned;
            }
            Err(fault) => {
                let outcome = match fault {
                    BackendFault::Unsupported => RawWindowControlOutcome::Unsupported,
                    BackendFault::Refused => RawWindowControlOutcome::Refused,
                    BackendFault::TargetVanished => RawWindowControlOutcome::TargetVanished,
                    BackendFault::MalformedWindowManagerData => {
                        RawWindowControlOutcome::MalformedWindowManagerData
                    }
                    BackendFault::BackendUnavailable => unreachable!(),
                };
                let _ignored = reply.send(Ok(RawWindowControlEvidence::without_observation(
                    request, outcome,
                )));
            }
        }
    }
}

fn reject_queued(ordinary: &Receiver<ActorRequest>, kind: WindowControlActorFailureKind) {
    while let Ok(request) = ordinary.try_recv() {
        match request {
            ActorRequest::Execute { reply, .. } => {
                let _ignored = reply.send(Err(WindowControlActorFailure::new(kind)));
            }
            ActorRequest::Capabilities { reply } => {
                let _ignored = reply.send(Err(WindowControlActorFailure::new(kind)));
            }
        }
    }
}

#[derive(Default)]
struct ActorControl {
    state: Mutex<ActorControlState>,
}

#[derive(Default)]
struct ActorControlState {
    shutdown: bool,
    closed: Option<WindowControlActorFailureKind>,
    waiters: Vec<SyncSender<std::result::Result<(), WindowControlActorFailure>>>,
}

impl ActorControl {
    fn enqueue_shutdown(&self) -> WindowControlReply<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let mut state = lock_mutex(&self.state);
        if let Some(kind) = state.closed {
            let _ignored = reply.send(Err(WindowControlActorFailure::new(kind)));
        } else if state.waiters.len() == MAX_SHUTDOWN_WAITERS {
            let _ignored = reply.send(Err(WindowControlActorFailure::new(
                WindowControlActorFailureKind::ControlQueueFull,
            )));
        } else {
            state.shutdown = true;
            state.waiters.push(reply);
        }
        WindowControlReply { receiver }
    }

    fn take_shutdown(
        &self,
    ) -> Option<Vec<SyncSender<std::result::Result<(), WindowControlActorFailure>>>> {
        let mut state = lock_mutex(&self.state);
        if !state.shutdown {
            return None;
        }
        state.shutdown = false;
        Some(std::mem::take(&mut state.waiters))
    }

    fn complete_shutdown(
        &self,
        mut waiters: Vec<SyncSender<std::result::Result<(), WindowControlActorFailure>>>,
    ) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_none() {
            state.closed = Some(WindowControlActorFailureKind::ActorStopped);
            waiters.append(&mut state.waiters);
        }
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Ok(()));
        }
    }

    fn close(&self, kind: WindowControlActorFailureKind) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(kind);
        state.shutdown = false;
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Err(WindowControlActorFailure::new(kind)));
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

#[cfg(test)]
mod tests;
