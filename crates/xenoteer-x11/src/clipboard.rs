//! Secret-aware, bounded X11 selection actor.
//!
//! This raw layer owns a dedicated X connection and hidden InputOnly window.
//! It does not authorize clipboard access or inject a paste chord. Payload
//! bytes are intentionally absent from errors, health, events, and Debug.

use core::fmt;
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use xenoteer_protocol::{
    MAX_CLIPBOARD_TARGETS, MAX_PASTE_OBSERVATION_TIMEOUT_MS, MAX_SELECTION_BYTES, SelectionName,
    SelectionTransferMode, SelectionTransferTerminal,
};

use crate::{Result, X11Error};

mod atoms;
mod receive;
mod send;
mod x11;

use x11::X11ClipboardBackend;

/// Largest direct property body before ICCCM INCR is required.
pub const CLIPBOARD_DIRECT_LIMIT_BYTES: usize = 256 * 1_024;
/// Chunk size used by Xenoteer-owned ICCCM INCR sends.
pub const CLIPBOARD_INCR_CHUNK_BYTES: usize = 64 * 1_024;
/// Deadline for one direct/INCR selection transaction.
pub const CLIPBOARD_TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum concurrent INCR sends for one requestor.
pub const MAX_INCR_TRANSFERS_PER_REQUESTOR: usize = 2;
/// Maximum concurrent INCR sends globally.
pub const MAX_INCR_TRANSFERS_GLOBAL: usize = 8;
/// Default bounded command-lane capacity.
pub const DEFAULT_CLIPBOARD_REQUEST_CAPACITY: usize = 64;

const ACTOR_BACKSTOP: Duration = Duration::from_millis(10);
const MAX_EVENTS_PER_TURN: usize = 64;
const MAX_SHUTDOWN_WAITERS: usize = 64;

/// Closed raw target vocabulary. No caller-directed atom interning occurs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RawClipboardTarget {
    /// ICCCM target discovery.
    Targets,
    /// Selection acquisition server timestamp.
    Timestamp,
    /// ICCCM pair-list conversion.
    Multiple,
    /// UTF-8 text.
    Utf8String,
    /// MIME UTF-8 text.
    TextPlainUtf8,
    /// MIME text with UTF-8 bytes in release one.
    TextPlain,
    /// ICCCM ISO-8859-1 text.
    String,
    /// PNG bytes.
    ImagePng,
    /// Opaque bytes.
    ApplicationOctetStream,
}

impl RawClipboardTarget {
    /// Canonical wire name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Targets => "TARGETS",
            Self::Timestamp => "TIMESTAMP",
            Self::Multiple => "MULTIPLE",
            Self::Utf8String => "UTF8_STRING",
            Self::TextPlainUtf8 => "text/plain;charset=utf-8",
            Self::TextPlain => "text/plain",
            Self::String => "STRING",
            Self::ImagePng => "image/png",
            Self::ApplicationOctetStream => "application/octet-stream",
        }
    }

    const fn is_content(self) -> bool {
        !matches!(self, Self::Targets | Self::Timestamp | Self::Multiple)
    }
}

/// Kind of secret content stored by the raw actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardPayloadKind {
    /// UTF-8 text with the required aliases and optional Latin-1 STRING.
    Utf8Text,
    /// Bytes served only for one reviewed binary target.
    Binary(RawClipboardTarget),
}

/// Secret selection body. Its Debug output contains only safe metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct ClipboardPayload {
    kind: ClipboardPayloadKind,
    bytes: Arc<[u8]>,
}

impl ClipboardPayload {
    /// Construct UTF-8 text bounded by the selection ceiling.
    pub fn utf8_text(value: impl Into<String>) -> std::result::Result<Self, ClipboardRequestError> {
        let bytes: Vec<u8> = value.into().into_bytes();
        validate_payload_len(bytes.len())?;
        Ok(Self {
            kind: ClipboardPayloadKind::Utf8Text,
            bytes: bytes.into(),
        })
    }

    /// Construct reviewed binary content bounded by the selection ceiling.
    pub fn binary(
        target: RawClipboardTarget,
        bytes: Vec<u8>,
    ) -> std::result::Result<Self, ClipboardRequestError> {
        if !matches!(
            target,
            RawClipboardTarget::ImagePng | RawClipboardTarget::ApplicationOctetStream
        ) {
            return Err(ClipboardRequestError::InvalidTarget);
        }
        validate_payload_len(bytes.len())?;
        Ok(Self {
            kind: ClipboardPayloadKind::Binary(target),
            bytes: bytes.into(),
        })
    }

    /// Safe content kind.
    #[must_use]
    pub const fn kind(&self) -> ClipboardPayloadKind {
        self.kind
    }

    /// Exact secret byte length.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Expose bytes only at an already-authorized effect boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }

    fn digest(&self) -> ClipboardContentDigest {
        sha256_digest(&self.bytes)
    }

    fn representation(&self, target: RawClipboardTarget) -> Option<Arc<[u8]>> {
        match self.kind {
            ClipboardPayloadKind::Utf8Text
                if matches!(
                    target,
                    RawClipboardTarget::Utf8String
                        | RawClipboardTarget::TextPlainUtf8
                        | RawClipboardTarget::TextPlain
                ) =>
            {
                Some(Arc::clone(&self.bytes))
            }
            ClipboardPayloadKind::Utf8Text if target == RawClipboardTarget::String => {
                latin1(&self.bytes).map(Into::into)
            }
            ClipboardPayloadKind::Binary(binary) if binary == target => {
                Some(Arc::clone(&self.bytes))
            }
            _ => None,
        }
    }

    fn advertised_targets(&self) -> Vec<RawClipboardTarget> {
        let mut targets = vec![
            RawClipboardTarget::Targets,
            RawClipboardTarget::Timestamp,
            RawClipboardTarget::Multiple,
        ];
        match self.kind {
            ClipboardPayloadKind::Utf8Text => {
                targets.extend([
                    RawClipboardTarget::Utf8String,
                    RawClipboardTarget::TextPlainUtf8,
                    RawClipboardTarget::TextPlain,
                ]);
                if latin1(&self.bytes).is_some() {
                    targets.push(RawClipboardTarget::String);
                }
            }
            ClipboardPayloadKind::Binary(target) => targets.push(target),
        }
        targets
    }
}

impl fmt::Debug for ClipboardPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardPayload")
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .finish()
    }
}

/// Source of one owned selection value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardOwnershipSource {
    /// Ordinary raw API set.
    Api,
    /// Temporary value used by later paste coordination.
    TemporaryPaste,
    /// Previous bounded value copied back after paste.
    RestoredSnapshot,
}

/// Raw set operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardSetRequest {
    /// CLIPBOARD or PRIMARY, independently.
    pub selection: SelectionName,
    /// Secret content.
    pub payload: ClipboardPayload,
    /// Safe source classification.
    pub source: ClipboardOwnershipSource,
}

/// Raw external-owner read operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardReadRawRequest {
    /// Selection to convert.
    pub selection: SelectionName,
    /// Ordered unique content targets; empty uses documented text preference.
    pub preferred_targets: Vec<RawClipboardTarget>,
    /// Whether invalid text bytes may be returned as binary.
    pub allow_binary_fallback: bool,
}

impl ClipboardReadRawRequest {
    /// Validate target count, uniqueness, and content-only vocabulary.
    pub fn validate(&self) -> std::result::Result<(), ClipboardRequestError> {
        if self.preferred_targets.len() > MAX_CLIPBOARD_TARGETS {
            return Err(ClipboardRequestError::TooManyTargets);
        }
        let mut unique = HashSet::with_capacity(self.preferred_targets.len());
        if self
            .preferred_targets
            .iter()
            .any(|target| !target.is_content() || !unique.insert(*target))
        {
            return Err(ClipboardRequestError::InvalidTarget);
        }
        Ok(())
    }
}

/// Request a content-free observation window for a later paste chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardPasteObservationRequest {
    /// Usually CLIPBOARD; PRIMARY remains separately selectable.
    pub selection: SelectionName,
    /// Bounded observation duration.
    pub timeout: Duration,
}

impl ClipboardPasteObservationRequest {
    fn validate(self) -> std::result::Result<(), ClipboardRequestError> {
        if self.timeout.is_zero()
            || self.timeout > Duration::from_millis(u64::from(MAX_PASTE_OBSERVATION_TIMEOUT_MS))
        {
            return Err(ClipboardRequestError::InvalidTimeout);
        }
        Ok(())
    }
}

/// Verified selection ownership evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardOwnershipEvidence {
    /// Independent selection.
    pub selection: SelectionName,
    /// Actor-local monotonic revision.
    pub revision: u64,
    /// Hidden owner window, or zero after clear.
    pub owner: u32,
    /// Valid server timestamp used for acquisition/clear.
    pub server_time: u32,
    /// GetSelectionOwner matched the requested final owner.
    pub verified: bool,
}

/// Content-free transfer evidence safe for events and diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSelectionTransferEvidence {
    /// Content target transferred.
    pub target: RawClipboardTarget,
    /// Direct or INCR path.
    pub transfer: SelectionTransferMode,
    /// Exact accumulated content bytes.
    pub content_length: u64,
    /// Content identity, whose Debug representation is redacted.
    pub sha256: ClipboardContentDigest,
    /// Whether an owner change was observed.
    pub owner_changed: bool,
    /// Whether the INCR zero terminator was observed/written.
    pub terminal_chunk_observed: bool,
    /// Completed or content-free failure.
    pub terminal: SelectionTransferTerminal,
}

/// Exact SHA-256 content identity with redacted Debug output.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ClipboardContentDigest([u8; 32]);

impl ClipboardContentDigest {
    /// Wrap an already-computed SHA-256 digest for trusted adapter/test evidence.
    #[must_use]
    pub const fn from_sha256_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Raw digest bytes for authorized persistence/evidence adaptation.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ClipboardContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClipboardContentDigest([REDACTED])")
    }
}

/// Successful raw clipboard read. Debug is safe because payload Debug redacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawClipboardReadResult {
    /// Selection read.
    pub selection: SelectionName,
    /// Actor-local owner revision.
    pub revision: u64,
    /// Secret bytes and safe representation kind.
    pub payload: ClipboardPayload,
    /// Content-free transfer evidence.
    pub evidence: RawSelectionTransferEvidence,
}

/// Content-free paste observation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawClipboardPasteObservation {
    /// Selection watched.
    pub selection: SelectionName,
    /// Whether a compatible request was observed.
    pub request_observed: bool,
    /// Ordered unique compatible targets requested.
    pub requested_targets: Vec<RawClipboardTarget>,
    /// First terminal compatible transfer, if any.
    pub transfer: Option<RawSelectionTransferEvidence>,
}

/// Request-shape failure rejected before admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ClipboardRequestError {
    /// Payload crossed 16 MiB.
    #[error("clipboard payload exceeds the fixed selection ceiling")]
    PayloadTooLarge,
    /// Target was internal, duplicated, or not valid for this payload.
    #[error("clipboard target is invalid")]
    InvalidTarget,
    /// Too many preferred targets were supplied.
    #[error("clipboard target list exceeds its fixed ceiling")]
    TooManyTargets,
    /// Observation timeout was zero or exceeded two seconds.
    #[error("clipboard observation timeout is invalid")]
    InvalidTimeout,
}

/// Stable secret-free request/actor failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardActorFailureKind {
    /// Selection has no external owner.
    SelectionHasNoOwner,
    /// No compatible target/conversion exists.
    TargetUnsupported,
    /// Selection owner changed mid-transfer.
    OwnerChanged,
    /// Peer announced or sent too much content.
    SelectionTooLarge,
    /// Transfer deadline elapsed.
    TransferTimeout,
    /// Peer violated direct, MULTIPLE, or INCR framing.
    ProtocolViolation,
    /// Requestor disappeared during an owned transfer.
    RequestorDestroyed,
    /// Ownership set/clear verification lost a race.
    OwnershipRace,
    /// X11 transport became unusable.
    BackendUnavailable,
    /// Actor was poisoned by a terminal backend failure.
    ActorPoisoned,
    /// Orderly shutdown began.
    ActorStopped,
    /// Worker panic was contained.
    ActorPanicked,
    /// Bounded shutdown waiter collection was full.
    ControlQueueFull,
}

/// Typed secret-free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("clipboard actor request failed: {kind:?}")]
pub struct ClipboardActorFailure {
    /// Stable failure category.
    pub kind: ClipboardActorFailureKind,
}

impl ClipboardActorFailure {
    const fn new(kind: ClipboardActorFailureKind) -> Self {
        Self { kind }
    }
}

/// Immediate bounded admission failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ClipboardSubmitError {
    /// Request shape failed before admission.
    #[error(transparent)]
    InvalidRequest(#[from] ClipboardRequestError),
    /// Ordinary lane is full.
    #[error("clipboard request queue is full")]
    QueueFull,
    /// Actor no longer admits requests.
    #[error("clipboard actor is closed")]
    Closed,
}

/// Lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardActorState {
    /// Backend is starting.
    Starting,
    /// Actor is servicing commands and X events.
    Healthy,
    /// Terminal backend failure ended service.
    Poisoned,
    /// Orderly shutdown completed.
    Stopped,
    /// Worker panic was contained.
    Panicked,
}

/// Secret-free actor health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardActorHealth {
    /// Lifecycle state.
    pub state: ClipboardActorState,
    /// Commands accepted by the backend.
    pub completed_commands: u64,
    /// Active outgoing INCR transfers.
    pub active_outgoing_incr: u8,
    /// Active external-owner reads.
    pub active_reads: u8,
    /// Last terminal actor failure.
    pub last_failure: Option<ClipboardActorFailureKind>,
}

/// Content-free actor event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardActorEvent {
    /// One or more content-free events were lost to bounded backpressure.
    ///
    /// Consumers must rebuild incremental state before trusting later events.
    ResyncRequired,
    /// Selection ownership/revision changed.
    OwnershipChanged {
        /// Independent selection.
        selection: SelectionName,
        /// Actor-local revision.
        revision: u64,
        /// Whether Xenoteer is the current owner.
        owned: bool,
    },
    /// One compatible request/transfer was observed for paste coordination.
    PasteObserved(RawClipboardPasteObservation),
    /// A content-free transfer failure occurred outside a command reply.
    TransferFailed {
        /// Independent selection.
        selection: SelectionName,
        /// Stable failure.
        failure: ClipboardActorFailureKind,
    },
}

/// Reply receiver with caller-selected wait.
#[derive(Debug)]
pub struct ClipboardReply<T> {
    receiver: Receiver<std::result::Result<T, ClipboardActorFailure>>,
}

impl<T> ClipboardReply<T> {
    /// Receive with a caller-selected timeout.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<std::result::Result<T, ClipboardActorFailure>, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

/// Paste observation whose ready signal proves the actor installed its watcher.
#[derive(Debug)]
pub struct ClipboardPasteObservation {
    ready: Receiver<std::result::Result<(), ClipboardActorFailure>>,
    reply: ClipboardReply<RawClipboardPasteObservation>,
}

impl ClipboardPasteObservation {
    /// Wait until the actor has installed the watcher, closing the enqueue/inject race.
    pub fn wait_until_ready(
        &self,
        timeout: Duration,
    ) -> std::result::Result<std::result::Result<(), ClipboardActorFailure>, RecvTimeoutError> {
        self.ready.recv_timeout(timeout)
    }

    /// Wait for terminal compatible direct/INCR evidence or the requested timeout.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<
        std::result::Result<RawClipboardPasteObservation, ClipboardActorFailure>,
        RecvTimeoutError,
    > {
        self.reply.recv_timeout(timeout)
    }
}

/// Bounded actor event receiver.
pub struct ClipboardActorEventReceiver {
    receiver: Receiver<ClipboardActorEvent>,
    event_loss: Arc<AtomicBool>,
}

impl ClipboardActorEventReceiver {
    /// Receive one event with a caller-selected timeout.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<ClipboardActorEvent, RecvTimeoutError> {
        match self.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Empty) => self.receiver.recv_timeout(timeout),
            Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// Attempts to receive without blocking the caller.
    pub fn try_recv(&self) -> std::result::Result<ClipboardActorEvent, TryRecvError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Empty) if self.event_loss.swap(false, Ordering::AcqRel) => {
                Ok(ClipboardActorEvent::ResyncRequired)
            }
            Err(error) => Err(error),
        }
    }
}

struct ClipboardEventSender {
    sender: SyncSender<ClipboardActorEvent>,
    event_loss: Arc<AtomicBool>,
}

impl ClipboardEventSender {
    fn emit(&self, event: ClipboardActorEvent) {
        if self.event_loss.load(Ordering::Acquire) {
            return;
        }
        if matches!(self.sender.try_send(event), Err(TrySendError::Full(_))) {
            self.event_loss.store(true, Ordering::Release);
        }
    }
}

enum ClipboardCommand {
    Set {
        request: ClipboardSetRequest,
        reply: SyncSender<std::result::Result<ClipboardOwnershipEvidence, ClipboardActorFailure>>,
    },
    Clear {
        selection: SelectionName,
        reply: SyncSender<std::result::Result<ClipboardOwnershipEvidence, ClipboardActorFailure>>,
    },
    Read {
        request: ClipboardReadRawRequest,
        reply: SyncSender<std::result::Result<RawClipboardReadResult, ClipboardActorFailure>>,
    },
    ObservePaste {
        request: ClipboardPasteObservationRequest,
        ready: SyncSender<std::result::Result<(), ClipboardActorFailure>>,
        reply: SyncSender<std::result::Result<RawClipboardPasteObservation, ClipboardActorFailure>>,
    },
}

impl ClipboardCommand {
    fn fail(self, kind: ClipboardActorFailureKind) {
        match self {
            Self::Set { reply, .. } | Self::Clear { reply, .. } => {
                let _ignored = reply.send(Err(ClipboardActorFailure::new(kind)));
            }
            Self::Read { reply, .. } => {
                let _ignored = reply.send(Err(ClipboardActorFailure::new(kind)));
            }
            Self::ObservePaste { ready, reply, .. } => {
                let failure = ClipboardActorFailure::new(kind);
                let _ignored = ready.send(Err(failure));
                let _ignored = reply.send(Err(failure));
            }
        }
    }
}

/// Cloneable bounded actor handle.
#[derive(Clone)]
pub struct ClipboardActorHandle {
    ordinary: SyncSender<ClipboardCommand>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
    health: Arc<RwLock<ClipboardActorHealth>>,
    wake: Arc<ActorWake>,
}

impl ClipboardActorHandle {
    /// Acquire and serve one selection value.
    pub fn try_set(
        &self,
        request: ClipboardSetRequest,
    ) -> std::result::Result<ClipboardReply<ClipboardOwnershipEvidence>, ClipboardSubmitError> {
        validate_payload_len(request.payload.byte_len())?;
        self.submit(|reply| ClipboardCommand::Set { request, reply })
    }

    /// Relinquish one selection without affecting the other.
    pub fn try_clear(
        &self,
        selection: SelectionName,
    ) -> std::result::Result<ClipboardReply<ClipboardOwnershipEvidence>, ClipboardSubmitError> {
        self.submit(|reply| ClipboardCommand::Clear { selection, reply })
    }

    /// Read one bounded representation from the current external owner.
    pub fn try_read(
        &self,
        request: ClipboardReadRawRequest,
    ) -> std::result::Result<ClipboardReply<RawClipboardReadResult>, ClipboardSubmitError> {
        request.validate()?;
        self.submit(|reply| ClipboardCommand::Read { request, reply })
    }

    /// Observe later selection consumption without injecting input.
    pub fn try_observe_paste(
        &self,
        request: ClipboardPasteObservationRequest,
    ) -> std::result::Result<ClipboardPasteObservation, ClipboardSubmitError> {
        request.validate()?;
        if !self.accepting.load(Ordering::Acquire) {
            return Err(ClipboardSubmitError::Closed);
        }
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (reply_tx, reply) = mpsc::sync_channel(1);
        self.ordinary
            .try_send(ClipboardCommand::ObservePaste {
                request,
                ready: ready_tx,
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => ClipboardSubmitError::QueueFull,
                TrySendError::Disconnected(_) => ClipboardSubmitError::Closed,
            })?;
        self.wake.notify();
        Ok(ClipboardPasteObservation {
            ready,
            reply: ClipboardReply { receiver: reply },
        })
    }

    /// Latest content-free health snapshot.
    #[must_use]
    pub fn health(&self) -> ClipboardActorHealth {
        *read_lock(&self.health)
    }

    /// Coalesce orderly shutdown on the capacity-independent control path.
    #[must_use]
    pub fn shutdown(&self) -> ClipboardReply<()> {
        self.accepting.store(false, Ordering::Release);
        self.control.enqueue_shutdown(&self.wake)
    }

    fn submit<T>(
        &self,
        build: impl FnOnce(
            SyncSender<std::result::Result<T, ClipboardActorFailure>>,
        ) -> ClipboardCommand,
    ) -> std::result::Result<ClipboardReply<T>, ClipboardSubmitError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(ClipboardSubmitError::Closed);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.ordinary
            .try_send(build(reply))
            .map_err(|error| match error {
                TrySendError::Full(_) => ClipboardSubmitError::QueueFull,
                TrySendError::Disconnected(_) => ClipboardSubmitError::Closed,
            })?;
        self.wake.notify();
        Ok(ClipboardReply { receiver })
    }
}

/// Owned actor join capability; joining initiates shutdown.
pub struct ClipboardActorJoin {
    thread: Option<JoinHandle<ClipboardActorExit>>,
    control: Arc<ActorControl>,
    accepting: Arc<AtomicBool>,
    wake: Arc<ActorWake>,
}

impl ClipboardActorJoin {
    /// Request shutdown and synchronously join.
    pub fn join(mut self) -> ClipboardActorExit {
        let Some(thread) = self.thread.take() else {
            return ClipboardActorExit::Stopped;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown(&self.wake);
        thread.join().unwrap_or(ClipboardActorExit::Panicked)
    }
}

impl Drop for ClipboardActorJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.accepting.store(false, Ordering::Release);
        let _reply = self.control.enqueue_shutdown(&self.wake);
        let _exit = thread.join();
    }
}

/// Terminal worker state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardActorExit {
    /// Orderly shutdown.
    Stopped,
    /// Terminal backend failure.
    Poisoned,
    /// Panic contained at actor boundary.
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendFault {
    Unavailable,
}

trait ClipboardBackend: Send + 'static {
    fn poll_event(
        &mut self,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<bool, BackendFault>;
    fn handle_command(
        &mut self,
        command: ClipboardCommand,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault>;
    fn expire(
        &mut self,
        now: Instant,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault>;
    fn counters(&self) -> (u8, u8);
    fn shutdown(&mut self, kind: ClipboardActorFailureKind);
}

/// Spawn the production clipboard actor. Connection/window creation happens on
/// the worker thread.
pub fn spawn_clipboard_actor(
    display: &str,
) -> Result<(
    ClipboardActorHandle,
    ClipboardActorEventReceiver,
    ClipboardActorJoin,
)> {
    let display = display.to_owned();
    spawn_with_backend(DEFAULT_CLIPBOARD_REQUEST_CAPACITY, 128, move || {
        X11ClipboardBackend::open(&display)
    })
}

fn spawn_with_backend<B, F>(
    request_capacity: usize,
    event_capacity: usize,
    factory: F,
) -> Result<(
    ClipboardActorHandle,
    ClipboardActorEventReceiver,
    ClipboardActorJoin,
)>
where
    B: ClipboardBackend,
    F: FnOnce() -> Result<B> + Send + 'static,
{
    if request_capacity == 0 || event_capacity == 0 {
        return Err(X11Error::InvalidSetup(
            "clipboard actor capacities must be positive",
        ));
    }
    let (ordinary_tx, ordinary_rx) = mpsc::sync_channel(request_capacity);
    let (event_tx, event_rx) = mpsc::sync_channel(event_capacity);
    let event_loss = Arc::new(AtomicBool::new(false));
    let accepting = Arc::new(AtomicBool::new(true));
    let health = Arc::new(RwLock::new(ClipboardActorHealth {
        state: ClipboardActorState::Starting,
        completed_commands: 0,
        active_outgoing_incr: 0,
        active_reads: 0,
        last_failure: None,
    }));
    let control = Arc::new(ActorControl::default());
    let wake = Arc::new(ActorWake::default());
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);

    let thread_accepting = Arc::clone(&accepting);
    let thread_health = Arc::clone(&health);
    let thread_control = Arc::clone(&control);
    let thread_wake = Arc::clone(&wake);
    let thread_event_loss = Arc::clone(&event_loss);
    let thread = thread::Builder::new()
        .name("xenoteer-clipboard-actor".to_owned())
        .spawn(move || {
            let backend = match catch_unwind(AssertUnwindSafe(factory)) {
                Ok(Ok(backend)) => backend,
                Ok(Err(error)) => {
                    let _ignored = startup_tx.send(Err(error));
                    return ClipboardActorExit::Stopped;
                }
                Err(_) => {
                    drop(startup_tx);
                    return ClipboardActorExit::Panicked;
                }
            };
            write_lock(&thread_health).state = ClipboardActorState::Healthy;
            let _ignored = startup_tx.send(Ok(()));
            match catch_unwind(AssertUnwindSafe(|| {
                run_actor(
                    backend,
                    ordinary_rx,
                    ClipboardEventSender {
                        sender: event_tx,
                        event_loss: thread_event_loss,
                    },
                    &thread_control,
                    &thread_health,
                    &thread_accepting,
                    &thread_wake,
                )
            })) {
                Ok(exit) => exit,
                Err(_) => {
                    thread_accepting.store(false, Ordering::Release);
                    thread_control.close(ClipboardActorFailureKind::ActorPanicked);
                    let mut health = write_lock(&thread_health);
                    health.state = ClipboardActorState::Panicked;
                    health.last_failure = Some(ClipboardActorFailureKind::ActorPanicked);
                    ClipboardActorExit::Panicked
                }
            }
        })
        .map_err(|error| X11Error::Poll(error.to_string()))?;

    match startup_rx.recv() {
        Ok(Ok(())) => Ok((
            ClipboardActorHandle {
                ordinary: ordinary_tx,
                control: Arc::clone(&control),
                accepting: Arc::clone(&accepting),
                health,
                wake: Arc::clone(&wake),
            },
            ClipboardActorEventReceiver {
                receiver: event_rx,
                event_loss,
            },
            ClipboardActorJoin {
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

fn run_actor<B: ClipboardBackend>(
    mut backend: B,
    ordinary: Receiver<ClipboardCommand>,
    events: ClipboardEventSender,
    control: &ActorControl,
    health: &RwLock<ClipboardActorHealth>,
    accepting: &AtomicBool,
    wake: &ActorWake,
) -> ClipboardActorExit {
    let mut wake_sequence = 0;
    loop {
        if let Some(waiters) = control.take_shutdown() {
            accepting.store(false, Ordering::Release);
            reject_queued(&ordinary, ClipboardActorFailureKind::ActorStopped);
            backend.shutdown(ClipboardActorFailureKind::ActorStopped);
            control.complete_shutdown(waiters);
            write_lock(health).state = ClipboardActorState::Stopped;
            return ClipboardActorExit::Stopped;
        }

        let mut did_work = false;
        for _ in 0..MAX_EVENTS_PER_TURN {
            match backend.poll_event(&events) {
                Ok(true) => did_work = true,
                Ok(false) => break,
                Err(BackendFault::Unavailable) => {
                    return poison(&mut backend, &ordinary, control, health, accepting);
                }
            }
        }
        if backend.expire(Instant::now(), &events).is_err() {
            return poison(&mut backend, &ordinary, control, health, accepting);
        }

        let (outgoing, reads) = backend.counters();
        {
            let mut snapshot = write_lock(health);
            snapshot.active_outgoing_incr = outgoing;
            snapshot.active_reads = reads;
        }

        match ordinary.try_recv() {
            Ok(command) => {
                did_work = true;
                if backend.handle_command(command, &events).is_err() {
                    return poison(&mut backend, &ordinary, control, health, accepting);
                }
                let mut snapshot = write_lock(health);
                snapshot.completed_commands = snapshot.completed_commands.saturating_add(1);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                backend.shutdown(ClipboardActorFailureKind::ActorStopped);
                control.close(ClipboardActorFailureKind::ActorStopped);
                write_lock(health).state = ClipboardActorState::Stopped;
                return ClipboardActorExit::Stopped;
            }
        }

        if !did_work {
            wake_sequence = wake.wait(wake_sequence, ACTOR_BACKSTOP);
        }
    }
}

fn poison<B: ClipboardBackend>(
    backend: &mut B,
    ordinary: &Receiver<ClipboardCommand>,
    control: &ActorControl,
    health: &RwLock<ClipboardActorHealth>,
    accepting: &AtomicBool,
) -> ClipboardActorExit {
    accepting.store(false, Ordering::Release);
    backend.shutdown(ClipboardActorFailureKind::ActorPoisoned);
    reject_queued(ordinary, ClipboardActorFailureKind::ActorPoisoned);
    control.close(ClipboardActorFailureKind::ActorPoisoned);
    let mut snapshot = write_lock(health);
    snapshot.state = ClipboardActorState::Poisoned;
    snapshot.last_failure = Some(ClipboardActorFailureKind::BackendUnavailable);
    ClipboardActorExit::Poisoned
}

fn reject_queued(ordinary: &Receiver<ClipboardCommand>, kind: ClipboardActorFailureKind) {
    while let Ok(command) = ordinary.try_recv() {
        command.fail(kind);
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
    closed: Option<ClipboardActorFailureKind>,
    waiters: Vec<SyncSender<std::result::Result<(), ClipboardActorFailure>>>,
}

impl ActorControl {
    fn enqueue_shutdown(&self, wake: &ActorWake) -> ClipboardReply<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let mut state = lock_mutex(&self.state);
        if let Some(kind) = state.closed {
            let _ignored = reply.send(Err(ClipboardActorFailure::new(kind)));
        } else if state.waiters.len() == MAX_SHUTDOWN_WAITERS {
            let _ignored = reply.send(Err(ClipboardActorFailure::new(
                ClipboardActorFailureKind::ControlQueueFull,
            )));
        } else {
            state.shutdown = true;
            state.waiters.push(reply);
            wake.notify();
        }
        ClipboardReply { receiver }
    }

    fn take_shutdown(
        &self,
    ) -> Option<Vec<SyncSender<std::result::Result<(), ClipboardActorFailure>>>> {
        let mut state = lock_mutex(&self.state);
        if !state.shutdown {
            return None;
        }
        state.shutdown = false;
        Some(std::mem::take(&mut state.waiters))
    }

    fn complete_shutdown(
        &self,
        mut waiters: Vec<SyncSender<std::result::Result<(), ClipboardActorFailure>>>,
    ) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_none() {
            state.closed = Some(ClipboardActorFailureKind::ActorStopped);
            waiters.append(&mut state.waiters);
        }
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Ok(()));
        }
    }

    fn close(&self, kind: ClipboardActorFailureKind) {
        let mut state = lock_mutex(&self.state);
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(kind);
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waiter in waiters {
            let _ignored = waiter.send(Err(ClipboardActorFailure::new(kind)));
        }
    }
}

fn validate_payload_len(len: usize) -> std::result::Result<(), ClipboardRequestError> {
    if u64::try_from(len).map_or(true, |len| len > MAX_SELECTION_BYTES) {
        Err(ClipboardRequestError::PayloadTooLarge)
    } else {
        Ok(())
    }
}

fn latin1(utf8: &[u8]) -> Option<Vec<u8>> {
    std::str::from_utf8(utf8)
        .ok()?
        .chars()
        .map(|scalar| u8::try_from(u32::from(scalar)).ok())
        .collect()
}

fn sha256_digest(bytes: &[u8]) -> ClipboardContentDigest {
    ClipboardContentDigest(Sha256::digest(bytes).into())
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
