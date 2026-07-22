//! Bounded single-owner capture actor and two-worker post-processing pool.

use core::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use xenoteer_protocol::{
    CaptureValidationError, CursorCaptureEvidence, MAX_SCREENSHOT_BYTES, MAX_SCREENSHOT_DIMENSION,
    MAX_SCREENSHOT_PIXELS, Rect, ScreenshotFormat, ScreenshotRequest, ScreenshotResizeFilter,
    ScreenshotTarget, Size, WindowRef,
};

use super::image::{CaptureImageLimits, ResizeFilter, encode_png_bgra8, resize_bgra8};
use crate::{Result, X11Error};

use super::x11::X11CaptureBackend;

/// Fixed ordinary screenshot admission capacity.
pub const CAPTURE_REQUEST_CAPACITY: usize = 16;
/// Maximum simultaneous resize/encode jobs.
pub const MAX_CAPTURE_ENCODE_JOBS: usize = 2;

const MAX_SHUTDOWN_WAITERS: usize = 64;
const ACTOR_BACKSTOP: Duration = Duration::from_millis(10);

/// Fresh X11 geometry established immediately after exact-reference revalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawWindowCaptureGeometry {
    /// Capture actor's root drawable.
    pub root: u32,
    /// Exact revalidated client drawable.
    pub window: u32,
    /// Client rectangle in root-physical coordinates.
    pub client_root: Rect,
    /// Window-manager frame rectangle when a distinct frame is established.
    pub frame_root: Option<Rect>,
    /// Core map state is exactly Viewable.
    pub viewable: bool,
}

/// Exact-reference revalidator failure, intentionally free of window metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RawCaptureRevalidationError {
    /// Reference no longer names the same observed XID birth.
    #[error("capture window reference is stale")]
    StaleReference,
    /// Exact birth is no longer live.
    #[error("capture window vanished")]
    TargetVanished,
    /// Authoritative observation service could not decide.
    #[error("capture revalidator is unavailable")]
    Unavailable,
}

pub(super) type Revalidator = Box<
    dyn FnOnce(&WindowRef) -> std::result::Result<(), RawCaptureRevalidationError> + Send + 'static,
>;

/// Truthful limitations attached to raw capture bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawCaptureLimitation {
    /// Root framebuffer represents visible pixels and includes occlusion.
    RootVisibleFramebuffer,
    /// Root crop represents the current visible rectangle, including occluders.
    WindowVisibleIncludesOccluders,
    /// Drawable pixels in obscured regions depend on X11 backing-store behavior.
    WindowDrawableObscuredUndefined,
}

/// Exact SHA-256 identity with redacted Debug output.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CaptureContentDigest([u8; 32]);

impl CaptureContentDigest {
    /// Raw digest bytes for later protocol/artifact adaptation.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for CaptureContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureContentDigest([REDACTED])")
    }
}

/// Screenshot body whose Debug representation never exposes pixels.
#[derive(Clone, Eq, PartialEq)]
pub struct RawCaptureBytes(Arc<[u8]>);

impl RawCaptureBytes {
    /// Exact body length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the encoded/raw body is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Expose bytes only at an already-authorized artifact/response boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RawCaptureBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawCaptureBytes")
            .field("bytes", &self.0.len())
            .field("pixels", &"[REDACTED]")
            .finish()
    }
}

/// Raw result for daemon-side artifact-only delivery adaptation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawCaptureResult {
    /// Exact admitted target.
    pub target: ScreenshotTarget,
    /// Actual output representation.
    pub format: ScreenshotFormat,
    /// Root-physical source region, including a drawable target's placement.
    pub source_region: Rect,
    /// Pre-scale capture size.
    pub source_size: Size,
    /// Final output dimensions.
    pub output_size: Size,
    /// Raw stride for BGRA, absent for PNG.
    pub raw_stride_bytes: Option<u32>,
    /// Weak XFIXES cursor evidence.
    pub cursor: CursorCaptureEvidence,
    /// Deliberate source limitation.
    pub limitation: RawCaptureLimitation,
    /// Exact body identity.
    pub sha256: CaptureContentDigest,
    /// Secret encoded or raw screenshot body.
    pub bytes: RawCaptureBytes,
}

/// Stable content-free capture failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureActorFailureKind {
    /// Root/window submission method did not match the request target.
    InvalidTarget,
    /// Monotonic deadline elapsed before completion.
    DeadlineExceeded,
    /// Caller cancellation was observed at a bounded boundary.
    Cancelled,
    /// Exact window reference became stale.
    StaleReference,
    /// Target disappeared during revalidation or fresh X queries.
    TargetVanished,
    /// Target is not currently viewable.
    WindowNotViewable,
    /// Crop is invalid or outside the selected source.
    RegionOutOfBounds,
    /// XFIXES cursor capture is unavailable or malformed.
    CursorUnavailable,
    /// Encoded/raw output crossed its admitted ceiling.
    OutputTooLarge,
    /// Resize/PNG work failed without exposing pixels.
    EncodeFailed,
    /// X11 transport is unusable.
    BackendUnavailable,
    /// Backend stopped ordinary service after a terminal failure.
    ActorPoisoned,
    /// Orderly shutdown began.
    ActorStopped,
    /// Worker panic was contained.
    ActorPanicked,
    /// Bounded control waiter collection is full.
    ControlQueueFull,
}

/// Typed content-free operation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("capture actor request failed: {kind:?}")]
pub struct CaptureActorFailure {
    /// Stable category.
    pub kind: CaptureActorFailureKind,
}

impl CaptureActorFailure {
    const fn new(kind: CaptureActorFailureKind) -> Self {
        Self { kind }
    }
}

/// Immediate pre-admission failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CaptureSubmitError {
    /// Protocol screenshot shape is invalid.
    #[error(transparent)]
    InvalidRequest(#[from] CaptureValidationError),
    /// Deadline has already elapsed.
    #[error("capture deadline has already elapsed")]
    InvalidDeadline,
    /// Bounded ordinary lane is full.
    #[error("capture request queue is full")]
    QueueFull,
    /// Actor no longer admits work.
    #[error("capture actor is closed")]
    Closed,
    /// Submission method and target kind disagree.
    #[error("capture submission target is invalid")]
    InvalidTarget,
}

/// Lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureActorState {
    /// Backend is starting.
    Starting,
    /// Actor is admitting bounded work.
    Healthy,
    /// Terminal X11 failure stopped service.
    Poisoned,
    /// Orderly shutdown completed.
    Stopped,
    /// Actor panic was contained.
    Panicked,
}

/// Content-free actor health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureActorHealth {
    /// Lifecycle.
    pub state: CaptureActorState,
    /// Commands accepted by the X owner thread.
    pub captured_commands: u64,
    /// Captures completed through MIT-SHM.
    pub shm_captures: u64,
    /// Captures completed through core `GetImage`.
    pub core_captures: u64,
    /// MIT-SHM attempts that safely fell back to core `GetImage`.
    pub shm_fallbacks: u64,
    /// Resize/encode jobs executing or queued, at most two.
    pub active_encode_jobs: u8,
    /// Last terminal failure.
    pub last_failure: Option<CaptureActorFailureKind>,
}

/// Caller-selected reply wait wrapper.
#[derive(Debug)]
pub struct CaptureReply<T> {
    receiver: Receiver<std::result::Result<T, CaptureActorFailure>>,
}

impl<T> CaptureReply<T> {
    /// Wait up to the caller's bound.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<std::result::Result<T, CaptureActorFailure>, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

struct CaptureCommand {
    request: ScreenshotRequest,
    deadline: Instant,
    cancellation: CancellationToken,
    revalidate: Option<Revalidator>,
    reply: SyncSender<std::result::Result<RawCaptureResult, CaptureActorFailure>>,
}

impl CaptureCommand {
    fn fail(self, kind: CaptureActorFailureKind) {
        let _ignored = self.reply.send(Err(CaptureActorFailure::new(kind)));
    }
}

/// Cloneable bounded actor handle.
#[derive(Clone)]
pub struct CaptureActorHandle {
    ordinary: SyncSender<CaptureCommand>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
    health: Arc<RwLock<CaptureActorHealth>>,
    wake: Arc<ActorWake>,
}

impl CaptureActorHandle {
    /// Submit a root framebuffer capture. Window targets are rejected.
    pub fn try_capture_root(
        &self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> std::result::Result<CaptureReply<RawCaptureResult>, CaptureSubmitError> {
        if !matches!(request.target, ScreenshotTarget::Root) {
            return Err(CaptureSubmitError::InvalidTarget);
        }
        self.submit(request, deadline, cancellation, None)
    }

    /// Submit a window capture with a mandatory exact-reference revalidator.
    pub fn try_capture_window<F>(
        &self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
        revalidate: F,
    ) -> std::result::Result<CaptureReply<RawCaptureResult>, CaptureSubmitError>
    where
        F: FnOnce(&WindowRef) -> std::result::Result<(), RawCaptureRevalidationError>
            + Send
            + 'static,
    {
        if matches!(request.target, ScreenshotTarget::Root) {
            return Err(CaptureSubmitError::InvalidTarget);
        }
        self.submit(request, deadline, cancellation, Some(Box::new(revalidate)))
    }

    /// Latest content-free health.
    #[must_use]
    pub fn health(&self) -> CaptureActorHealth {
        *read_lock(&self.health)
    }

    /// Coalesce shutdown through the capacity-independent control path.
    #[must_use]
    pub fn shutdown(&self) -> CaptureReply<()> {
        self.accepting.store(false, Ordering::Release);
        self.control.enqueue_shutdown(&self.wake)
    }

    fn submit(
        &self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
        revalidate: Option<Revalidator>,
    ) -> std::result::Result<CaptureReply<RawCaptureResult>, CaptureSubmitError> {
        request.validate()?;
        if deadline <= Instant::now() {
            return Err(CaptureSubmitError::InvalidDeadline);
        }
        if !self.accepting.load(Ordering::Acquire) {
            return Err(CaptureSubmitError::Closed);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.ordinary
            .try_send(CaptureCommand {
                request,
                deadline,
                cancellation,
                revalidate,
                reply,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => CaptureSubmitError::QueueFull,
                TrySendError::Disconnected(_) => CaptureSubmitError::Closed,
            })?;
        self.wake.notify();
        Ok(CaptureReply { receiver })
    }
}

/// Owned join capability; joining initiates shutdown.
pub struct CaptureActorJoin {
    thread: Option<JoinHandle<CaptureActorExit>>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
    wake: Arc<ActorWake>,
}

impl CaptureActorJoin {
    /// Request shutdown and synchronously join the actor and encode workers.
    pub fn join(mut self) -> CaptureActorExit {
        let Some(thread) = self.thread.take() else {
            return CaptureActorExit::Stopped;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown(&self.wake);
        thread.join().unwrap_or(CaptureActorExit::Panicked)
    }
}

impl Drop for CaptureActorJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown(&self.wake);
        let _exit = thread.join();
    }
}

/// Terminal actor state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureActorExit {
    /// Orderly shutdown.
    Stopped,
    /// Terminal X11 failure.
    Poisoned,
    /// Panic contained.
    Panicked,
}

pub(super) struct CapturedFrame {
    pub target: ScreenshotTarget,
    pub source_region: Rect,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub cursor: CursorCaptureEvidence,
    pub limitation: RawCaptureLimitation,
    pub transport: RawCaptureTransport,
    pub shm_fallback: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RawCaptureTransport {
    CoreGetImage,
    MitShm,
}

impl fmt::Debug for CapturedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedFrame")
            .field("target", &self.target)
            .field("source_region", &self.source_region)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bgra_bytes", &self.bgra.len())
            .field("pixels", &"[REDACTED]")
            .field("cursor", &self.cursor)
            .field("limitation", &self.limitation)
            .field("transport", &self.transport)
            .field("shm_fallback", &self.shm_fallback)
            .finish()
    }
}

pub(super) enum CaptureBackendError {
    Operation(CaptureActorFailureKind),
    Unavailable,
}

pub(super) trait CaptureBackend: Send + 'static {
    fn capture(
        &mut self,
        request: &ScreenshotRequest,
        revalidate: Option<Revalidator>,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError>;
    fn shutdown(&mut self);
}

/// Spawn the production capture actor with one distinct X connection.
pub fn spawn_capture_actor(display: &str) -> Result<(CaptureActorHandle, CaptureActorJoin)> {
    let display = display.to_owned();
    spawn_with_backend(move || X11CaptureBackend::open(&display))
}

fn spawn_with_backend<B, F>(factory: F) -> Result<(CaptureActorHandle, CaptureActorJoin)>
where
    B: CaptureBackend,
    F: FnOnce() -> Result<B> + Send + 'static,
{
    let (ordinary_tx, ordinary_rx) = mpsc::sync_channel(CAPTURE_REQUEST_CAPACITY);
    let accepting = Arc::new(AtomicBool::new(true));
    let control = Arc::new(ActorControl::default());
    let health = Arc::new(RwLock::new(CaptureActorHealth {
        state: CaptureActorState::Starting,
        captured_commands: 0,
        shm_captures: 0,
        core_captures: 0,
        shm_fallbacks: 0,
        active_encode_jobs: 0,
        last_failure: None,
    }));
    let wake = Arc::new(ActorWake::default());
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let thread_accepting = Arc::clone(&accepting);
    let thread_control = Arc::clone(&control);
    let thread_health = Arc::clone(&health);
    let thread_wake = Arc::clone(&wake);
    let thread = thread::Builder::new()
        .name("xenoteer-capture-actor".to_owned())
        .spawn(move || {
            let mut backend = match catch_unwind(AssertUnwindSafe(factory)) {
                Ok(Ok(backend)) => backend,
                Ok(Err(error)) => {
                    let _ignored = startup_tx.send(Err(error));
                    return CaptureActorExit::Stopped;
                }
                Err(_) => {
                    drop(startup_tx);
                    return CaptureActorExit::Panicked;
                }
            };
            let mut encoder = match EncoderPool::spawn(Arc::clone(&thread_wake)) {
                Ok(encoder) => encoder,
                Err(error) => {
                    let _ignored = startup_tx.send(Err(error));
                    return CaptureActorExit::Stopped;
                }
            };
            write_lock(&thread_health).state = CaptureActorState::Healthy;
            let _ignored = startup_tx.send(Ok(()));
            match catch_unwind(AssertUnwindSafe(|| {
                run_actor(
                    &mut backend,
                    &mut encoder,
                    ordinary_rx,
                    &thread_control,
                    &thread_health,
                    &thread_accepting,
                    &thread_wake,
                )
            })) {
                Ok(exit) => exit,
                Err(_) => {
                    backend.shutdown();
                    encoder.shutdown(CaptureActorFailureKind::ActorPanicked);
                    thread_accepting.store(false, Ordering::Release);
                    thread_control.close(CaptureActorFailureKind::ActorPanicked);
                    let mut snapshot = write_lock(&thread_health);
                    snapshot.state = CaptureActorState::Panicked;
                    snapshot.active_encode_jobs = 0;
                    snapshot.last_failure = Some(CaptureActorFailureKind::ActorPanicked);
                    CaptureActorExit::Panicked
                }
            }
        })
        .map_err(|error| X11Error::Poll(error.to_string()))?;

    match startup_rx.recv() {
        Ok(Ok(())) => Ok((
            CaptureActorHandle {
                ordinary: ordinary_tx,
                control: Arc::clone(&control),
                accepting: Arc::clone(&accepting),
                health,
                wake: Arc::clone(&wake),
            },
            CaptureActorJoin {
                thread: Some(thread),
                control,
                accepting,
                wake,
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

fn run_actor<B: CaptureBackend>(
    backend: &mut B,
    encoder: &mut EncoderPool,
    ordinary: Receiver<CaptureCommand>,
    control: &ActorControl,
    health: &RwLock<CaptureActorHealth>,
    accepting: &AtomicBool,
    wake: &ActorWake,
) -> CaptureActorExit {
    let mut wake_sequence = 0;
    loop {
        if let Some(waiters) = control.take_shutdown() {
            accepting.store(false, Ordering::Release);
            reject_queued(&ordinary, CaptureActorFailureKind::ActorStopped);
            backend.shutdown();
            encoder.shutdown(CaptureActorFailureKind::ActorStopped);
            control.complete_shutdown(waiters);
            let mut snapshot = write_lock(health);
            snapshot.state = CaptureActorState::Stopped;
            snapshot.active_encode_jobs = 0;
            return CaptureActorExit::Stopped;
        }
        write_lock(health).active_encode_jobs = encoder.active_jobs();
        if encoder.has_capacity() {
            match ordinary.try_recv() {
                Ok(mut command) => {
                    if command.cancellation.is_cancelled() {
                        command.fail(CaptureActorFailureKind::Cancelled);
                        continue;
                    }
                    if Instant::now() >= command.deadline {
                        command.fail(CaptureActorFailureKind::DeadlineExceeded);
                        continue;
                    }
                    let capture = catch_unwind(AssertUnwindSafe(|| {
                        backend.capture(&command.request, command.revalidate.take())
                    }));
                    let frame = match capture {
                        Ok(Ok(frame)) => frame,
                        Ok(Err(CaptureBackendError::Operation(kind))) => {
                            command.fail(kind);
                            continue;
                        }
                        Ok(Err(CaptureBackendError::Unavailable)) => {
                            command.fail(CaptureActorFailureKind::BackendUnavailable);
                            return poison(backend, encoder, &ordinary, control, health, accepting);
                        }
                        Err(_) => {
                            command.fail(CaptureActorFailureKind::ActorPanicked);
                            return panicked(
                                backend, encoder, &ordinary, control, health, accepting,
                            );
                        }
                    };
                    {
                        let mut snapshot = write_lock(health);
                        snapshot.captured_commands = snapshot.captured_commands.saturating_add(1);
                        match frame.transport {
                            RawCaptureTransport::CoreGetImage => {
                                snapshot.core_captures = snapshot.core_captures.saturating_add(1);
                            }
                            RawCaptureTransport::MitShm => {
                                snapshot.shm_captures = snapshot.shm_captures.saturating_add(1);
                            }
                        }
                        if frame.shm_fallback {
                            snapshot.shm_fallbacks = snapshot.shm_fallbacks.saturating_add(1);
                        }
                    }
                    if !encoder.try_dispatch(EncoderJob {
                        frame,
                        request: command.request,
                        deadline: command.deadline,
                        cancellation: command.cancellation,
                        reply: command.reply,
                    }) {
                        return poison(backend, encoder, &ordinary, control, health, accepting);
                    }
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    backend.shutdown();
                    encoder.shutdown(CaptureActorFailureKind::ActorStopped);
                    control.close(CaptureActorFailureKind::ActorStopped);
                    let mut snapshot = write_lock(health);
                    snapshot.state = CaptureActorState::Stopped;
                    snapshot.active_encode_jobs = 0;
                    return CaptureActorExit::Stopped;
                }
            }
        }
        wake_sequence = wake.wait(wake_sequence, ACTOR_BACKSTOP);
    }
}

fn poison<B: CaptureBackend>(
    backend: &mut B,
    encoder: &mut EncoderPool,
    ordinary: &Receiver<CaptureCommand>,
    control: &ActorControl,
    health: &RwLock<CaptureActorHealth>,
    accepting: &AtomicBool,
) -> CaptureActorExit {
    accepting.store(false, Ordering::Release);
    backend.shutdown();
    encoder.shutdown(CaptureActorFailureKind::ActorPoisoned);
    reject_queued(ordinary, CaptureActorFailureKind::ActorPoisoned);
    control.close(CaptureActorFailureKind::ActorPoisoned);
    let mut snapshot = write_lock(health);
    snapshot.state = CaptureActorState::Poisoned;
    snapshot.active_encode_jobs = 0;
    snapshot.last_failure = Some(CaptureActorFailureKind::BackendUnavailable);
    CaptureActorExit::Poisoned
}

fn panicked<B: CaptureBackend>(
    backend: &mut B,
    encoder: &mut EncoderPool,
    ordinary: &Receiver<CaptureCommand>,
    control: &ActorControl,
    health: &RwLock<CaptureActorHealth>,
    accepting: &AtomicBool,
) -> CaptureActorExit {
    accepting.store(false, Ordering::Release);
    backend.shutdown();
    encoder.shutdown(CaptureActorFailureKind::ActorPanicked);
    reject_queued(ordinary, CaptureActorFailureKind::ActorPanicked);
    control.close(CaptureActorFailureKind::ActorPanicked);
    let mut snapshot = write_lock(health);
    snapshot.state = CaptureActorState::Panicked;
    snapshot.active_encode_jobs = 0;
    snapshot.last_failure = Some(CaptureActorFailureKind::ActorPanicked);
    CaptureActorExit::Panicked
}

fn reject_queued(ordinary: &Receiver<CaptureCommand>, kind: CaptureActorFailureKind) {
    while let Ok(command) = ordinary.try_recv() {
        command.fail(kind);
    }
}

struct EncoderJob {
    frame: CapturedFrame,
    request: ScreenshotRequest,
    deadline: Instant,
    cancellation: CancellationToken,
    reply: SyncSender<std::result::Result<RawCaptureResult, CaptureActorFailure>>,
}

struct EncoderPool {
    sender: Option<SyncSender<EncoderJob>>,
    workers: Vec<JoinHandle<()>>,
    active: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
    stop_kind: Arc<Mutex<CaptureActorFailureKind>>,
}

impl EncoderPool {
    fn spawn(wake: Arc<ActorWake>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(MAX_CAPTURE_ENCODE_JOBS);
        let receiver = Arc::new(Mutex::new(receiver));
        let active = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_kind = Arc::new(Mutex::new(CaptureActorFailureKind::ActorStopped));
        let mut workers = Vec::with_capacity(MAX_CAPTURE_ENCODE_JOBS);
        for index in 0..MAX_CAPTURE_ENCODE_JOBS {
            let worker_receiver = Arc::clone(&receiver);
            let worker_active = Arc::clone(&active);
            let worker_stopping = Arc::clone(&stopping);
            let worker_stop_kind = Arc::clone(&stop_kind);
            let worker_wake = Arc::clone(&wake);
            match thread::Builder::new()
                .name(format!("xenoteer-capture-encode-{index}"))
                .spawn(move || {
                    encoder_worker(
                        &worker_receiver,
                        &worker_active,
                        &worker_stopping,
                        &worker_stop_kind,
                        &worker_wake,
                    );
                }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    stopping.store(true, Ordering::Release);
                    drop(sender);
                    for worker in workers {
                        let _exit = worker.join();
                    }
                    return Err(X11Error::Poll(error.to_string()));
                }
            }
        }
        Ok(Self {
            sender: Some(sender),
            workers,
            active,
            stopping,
            stop_kind,
        })
    }

    fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Acquire) < MAX_CAPTURE_ENCODE_JOBS
    }

    fn active_jobs(&self) -> u8 {
        u8::try_from(self.active.load(Ordering::Acquire)).unwrap_or(u8::MAX)
    }

    fn try_dispatch(&self, job: EncoderJob) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        self.active.fetch_add(1, Ordering::AcqRel);
        match sender.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.active.fetch_sub(1, Ordering::AcqRel);
                false
            }
        }
    }

    fn shutdown(&mut self, kind: CaptureActorFailureKind) {
        *lock_mutex(&self.stop_kind) = kind;
        self.stopping.store(true, Ordering::Release);
        drop(self.sender.take());
        for worker in self.workers.drain(..) {
            let _exit = worker.join();
        }
        self.active.store(0, Ordering::Release);
    }
}

fn encoder_worker(
    receiver: &Mutex<Receiver<EncoderJob>>,
    active: &AtomicUsize,
    stopping: &AtomicBool,
    stop_kind: &Mutex<CaptureActorFailureKind>,
    wake: &ActorWake,
) {
    loop {
        let job = {
            let receiver = lock_mutex(receiver);
            receiver.recv()
        };
        let Ok(job) = job else {
            return;
        };
        let result = if stopping.load(Ordering::Acquire) {
            Err(CaptureActorFailure::new(*lock_mutex(stop_kind)))
        } else {
            catch_unwind(AssertUnwindSafe(|| process_frame(&job))).unwrap_or_else(|_| {
                Err(CaptureActorFailure::new(
                    CaptureActorFailureKind::ActorPanicked,
                ))
            })
        };
        let result = if stopping.load(Ordering::Acquire) {
            Err(CaptureActorFailure::new(*lock_mutex(stop_kind)))
        } else {
            result
        };
        let _ignored = job.reply.send(result);
        active.fetch_sub(1, Ordering::AcqRel);
        wake.notify();
    }
}

fn process_frame(job: &EncoderJob) -> std::result::Result<RawCaptureResult, CaptureActorFailure> {
    check_job_boundary(job)?;
    let source_size = Size::new(job.frame.width, job.frame.height)
        .map_err(|_| CaptureActorFailure::new(CaptureActorFailureKind::EncodeFailed))?;
    validate_captured_frame(job, source_size)?;
    let output_size = job
        .request
        .validate_for_source(source_size)
        .map_err(map_validation_failure)?;
    let limits = CaptureImageLimits {
        max_dimension: MAX_SCREENSHOT_DIMENSION,
        max_pixels: MAX_SCREENSHOT_PIXELS,
        max_encoded_bytes: usize::try_from(job.request.max_bytes.unwrap_or(MAX_SCREENSHOT_BYTES))
            .unwrap_or(usize::MAX),
    };
    let resized;
    let pixels = if output_size == source_size {
        job.frame.bgra.as_slice()
    } else {
        let filter = match job.request.scale.map(|scale| scale.filter) {
            Some(ScreenshotResizeFilter::Nearest) => ResizeFilter::Nearest,
            Some(ScreenshotResizeFilter::Lanczos) | None => ResizeFilter::Lanczos3,
        };
        resized = resize_bgra8(
            job.frame.width,
            job.frame.height,
            &job.frame.bgra,
            output_size.width(),
            output_size.height(),
            filter,
            limits,
        )
        .map_err(|_| CaptureActorFailure::new(CaptureActorFailureKind::EncodeFailed))?;
        resized.as_slice()
    };
    check_job_boundary(job)?;
    let ceiling = usize::try_from(job.request.max_bytes.unwrap_or(MAX_SCREENSHOT_BYTES))
        .unwrap_or(usize::MAX);
    let (bytes, raw_stride_bytes) = match job.request.format {
        ScreenshotFormat::RawBgra => {
            if pixels.len() > ceiling {
                return Err(CaptureActorFailure::new(
                    CaptureActorFailureKind::OutputTooLarge,
                ));
            }
            (
                pixels.to_vec(),
                Some(output_size.width().checked_mul(4).ok_or_else(|| {
                    CaptureActorFailure::new(CaptureActorFailureKind::EncodeFailed)
                })?),
            )
        }
        ScreenshotFormat::Png => (
            encode_png_bgra8(output_size.width(), output_size.height(), pixels, limits)
                .map_err(|_| CaptureActorFailure::new(CaptureActorFailureKind::OutputTooLarge))?,
            None,
        ),
    };
    check_job_boundary(job)?;
    let sha256 = CaptureContentDigest(Sha256::digest(&bytes).into());
    Ok(RawCaptureResult {
        target: job.frame.target.clone(),
        format: job.request.format,
        source_region: job.frame.source_region,
        source_size,
        output_size,
        raw_stride_bytes,
        cursor: job.frame.cursor,
        limitation: job.frame.limitation,
        sha256,
        bytes: RawCaptureBytes(bytes.into()),
    })
}

fn validate_captured_frame(
    job: &EncoderJob,
    source_size: Size,
) -> std::result::Result<(), CaptureActorFailure> {
    let invalid = || CaptureActorFailure::new(CaptureActorFailureKind::EncodeFailed);
    let region_size = job.frame.source_region.size().map_err(|_| invalid())?;
    let expected_bytes = u64::from(source_size.width())
        .checked_mul(u64::from(source_size.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(invalid)?;
    let limitation_matches = matches!(
        (&job.request.target, job.frame.limitation),
        (
            ScreenshotTarget::Root,
            RawCaptureLimitation::RootVisibleFramebuffer
        ) | (
            ScreenshotTarget::WindowVisible { .. },
            RawCaptureLimitation::WindowVisibleIncludesOccluders
        ) | (
            ScreenshotTarget::WindowDrawable { .. },
            RawCaptureLimitation::WindowDrawableObscuredUndefined
        )
    );
    if job.frame.target != job.request.target
        || region_size != source_size
        || job.frame.bgra.len() != expected_bytes
        || !limitation_matches
        || job.frame.cursor.validate().is_err()
        || job.frame.cursor.requested != job.request.include_cursor
    {
        return Err(invalid());
    }
    Ok(())
}

fn check_job_boundary(job: &EncoderJob) -> std::result::Result<(), CaptureActorFailure> {
    if job.cancellation.is_cancelled() {
        Err(CaptureActorFailure::new(CaptureActorFailureKind::Cancelled))
    } else if Instant::now() >= job.deadline {
        Err(CaptureActorFailure::new(
            CaptureActorFailureKind::DeadlineExceeded,
        ))
    } else {
        Ok(())
    }
}

fn map_validation_failure(error: CaptureValidationError) -> CaptureActorFailure {
    let kind = match error {
        CaptureValidationError::OutputBytes => CaptureActorFailureKind::OutputTooLarge,
        _ => CaptureActorFailureKind::EncodeFailed,
    };
    CaptureActorFailure::new(kind)
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
        let (sequence, _) = self
            .condvar
            .wait_timeout(sequence, timeout)
            .unwrap_or_else(|error| error.into_inner());
        *sequence
    }
}

#[derive(Default)]
struct ActorControl {
    state: Mutex<ControlState>,
}

#[derive(Default)]
struct ControlState {
    shutdown: bool,
    closed: Option<CaptureActorFailureKind>,
    waiters: Vec<SyncSender<std::result::Result<(), CaptureActorFailure>>>,
}

impl ActorControl {
    fn enqueue_shutdown(&self, wake: &ActorWake) -> CaptureReply<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let mut state = lock_mutex(&self.state);
        if let Some(kind) = state.closed {
            let _ignored = reply.send(Err(CaptureActorFailure::new(kind)));
        } else if state.waiters.len() == MAX_SHUTDOWN_WAITERS {
            let _ignored = reply.send(Err(CaptureActorFailure::new(
                CaptureActorFailureKind::ControlQueueFull,
            )));
        } else {
            state.shutdown = true;
            state.waiters.push(reply);
            wake.notify();
        }
        CaptureReply { receiver }
    }

    fn take_shutdown(
        &self,
    ) -> Option<Vec<SyncSender<std::result::Result<(), CaptureActorFailure>>>> {
        let mut state = lock_mutex(&self.state);
        if !state.shutdown {
            return None;
        }
        state.shutdown = false;
        Some(std::mem::take(&mut state.waiters))
    }

    fn complete_shutdown(
        &self,
        mut waiters: Vec<SyncSender<std::result::Result<(), CaptureActorFailure>>>,
    ) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_none() {
            state.closed = Some(CaptureActorFailureKind::ActorStopped);
            waiters.append(&mut state.waiters);
        }
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Ok(()));
        }
    }

    fn close(&self, kind: CaptureActorFailureKind) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(kind);
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Err(CaptureActorFailure::new(kind)));
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
#[path = "actor_tests.rs"]
mod tests;
