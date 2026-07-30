//! Single-owner asynchronous AT-SPI actor, lifecycle, and bounded event ingress.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::mpsc as std_mpsc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::cache::{
    BoundedCache, CacheError, CacheEvent, CacheLimits, CacheMutationDetail, CacheMutationKind,
    CachePage, ObjectAddress, RefreshedCacheItem,
};
use crate::semantic::{
    BackendObservationRequest, BackendSemanticRequest, SemanticDispatchMarker,
    SemanticDispatchPermit, SemanticError, SemanticObservationRequest, SemanticObservationResult,
    SemanticReconcileResult, SemanticRequest, SemanticResult, SemanticTarget,
    SemanticTargetRequest, TextProtection,
};

const MAX_FAILURE_BYTES: usize = 4 * 1_024;

/// Boxed future used by backend seams without requiring an async-trait macro.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Independent health state of the semantic automation plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtspiActorState {
    /// Accessibility was disabled by configuration.
    Disabled,
    /// The initial connection/bootstrap is in progress.
    Connecting,
    /// The connection and bounded cache are usable.
    Healthy,
    /// The connection/cache was invalidated and retry is in progress.
    Reconnecting,
    /// Explicit shutdown completed.
    Stopped,
    /// The actor task panicked; it must never be restarted in place.
    Panicked,
}

/// Current generation-fenced actor health.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtspiActorHealth {
    /// Semantic plane lifecycle state.
    pub state: AtspiActorState,
    /// Generation invalidated on every established-bus/cache loss.
    pub accessibility_generation: u64,
    /// Monotonic cache revision within this actor process.
    pub cache_revision: u64,
    /// Current live normalized node count.
    pub cached_nodes: usize,
    /// Estimated normalized bytes retained by live cache items.
    pub cached_bytes: usize,
    /// Sanitized most recent backend failure.
    pub last_error: Option<String>,
}

impl AtspiActorHealth {
    fn initial(enabled: bool) -> Self {
        Self {
            state: if enabled {
                AtspiActorState::Connecting
            } else {
                AtspiActorState::Disabled
            },
            accessibility_generation: 1,
            cache_revision: 0,
            cached_nodes: 0,
            cached_bytes: 0,
            last_error: None,
        }
    }
}

/// Resource and deadline policy fixed at actor creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtspiActorConfig {
    /// Public request queue capacity.
    pub request_capacity: usize,
    /// Backend signal queue capacity.
    pub backend_event_capacity: usize,
    /// Public normalized event queue capacity.
    pub event_capacity: usize,
    /// Maximum nodes copied by one cache read request.
    pub read_page_nodes: usize,
    /// Maximum normalized bytes copied by one cache read request.
    pub read_page_bytes: usize,
    /// Whole connection attempt timeout.
    pub connect_timeout: Duration,
    /// Whole cache bootstrap timeout.
    pub bootstrap_timeout: Duration,
    /// Maximum duration of each application/registry proxy call.
    pub proxy_call_timeout: Duration,
    /// Backend shutdown deadline.
    pub shutdown_timeout: Duration,
    /// First reconnect delay.
    pub reconnect_initial: Duration,
    /// Maximum reconnect delay.
    pub reconnect_max: Duration,
    /// Cache admission ceilings.
    pub cache_limits: CacheLimits,
}

impl AtspiActorConfig {
    /// Validate queue, cache, deadline, and reconnect invariants.
    pub fn validate(self) -> Result<Self, AtspiActorSpawnError> {
        for (resource, value) in [
            ("request queue", self.request_capacity),
            ("backend event queue", self.backend_event_capacity),
            ("public event queue", self.event_capacity),
            ("cache read page nodes", self.read_page_nodes),
            ("cache read page bytes", self.read_page_bytes),
        ] {
            if value == 0 {
                return Err(AtspiActorSpawnError::InvalidConfig(resource));
            }
        }
        for (resource, value) in [
            ("connect timeout", self.connect_timeout),
            ("bootstrap timeout", self.bootstrap_timeout),
            ("proxy call timeout", self.proxy_call_timeout),
            ("shutdown timeout", self.shutdown_timeout),
            ("initial reconnect delay", self.reconnect_initial),
            ("maximum reconnect delay", self.reconnect_max),
        ] {
            if value.is_zero() {
                return Err(AtspiActorSpawnError::InvalidConfig(resource));
            }
        }
        if self.reconnect_initial > self.reconnect_max {
            return Err(AtspiActorSpawnError::InvalidConfig(
                "initial reconnect delay exceeds maximum",
            ));
        }
        self.cache_limits
            .validate()
            .map_err(AtspiActorSpawnError::Cache)?;
        let byte_weighted_queue_ceiling = self
            .cache_limits
            .max_total_bytes
            .checked_div(self.cache_limits.max_item_bytes)
            .unwrap_or(0)
            .max(1);
        for (resource, value) in [
            ("request queue byte-derived capacity", self.request_capacity),
            (
                "backend event queue byte-derived capacity",
                self.backend_event_capacity,
            ),
            (
                "public event queue byte-derived capacity",
                self.event_capacity,
            ),
        ] {
            if value > byte_weighted_queue_ceiling {
                return Err(AtspiActorSpawnError::InvalidConfig(resource));
            }
        }
        Ok(self)
    }
}

impl Default for AtspiActorConfig {
    fn default() -> Self {
        Self {
            request_capacity: 128,
            backend_event_capacity: 256,
            event_capacity: 256,
            read_page_nodes: 1_000,
            read_page_bytes: 16 * 1_024 * 1_024,
            connect_timeout: Duration::from_secs(2),
            bootstrap_timeout: Duration::from_secs(10),
            proxy_call_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
            reconnect_initial: Duration::from_millis(100),
            reconnect_max: Duration::from_secs(5),
            cache_limits: CacheLimits::default(),
        }
    }
}

/// Sanitized backend failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendFailureKind {
    /// Session or accessibility bus connection failed.
    Connection,
    /// Toolkit or bus data violated the supported protocol.
    Protocol,
    /// A backend operation exceeded its actor-owned deadline.
    Timeout,
    /// The dedicated signal stream terminated unexpectedly.
    Stream,
    /// Requested live action did not exist.
    ActionNotFound,
    /// Requested/default live action matched more than once.
    AmbiguousAction,
}

/// Sanitized failure returned across the backend seam.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct BackendFailure {
    /// Stable failure category.
    pub kind: BackendFailureKind,
    /// Bounded diagnostic text with no request secrets.
    pub message: String,
}

impl BackendFailure {
    /// Construct a failure while bounding untrusted diagnostic text.
    #[must_use]
    pub fn new(kind: BackendFailureKind, message: impl Into<String>) -> Self {
        let message = message.into();
        let message = if message.len() <= MAX_FAILURE_BYTES {
            message
        } else {
            let mut boundary = MAX_FAILURE_BYTES;
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message[..boundary].to_owned()
        };
        Self { kind, message }
    }
}

/// Signal normalized by a backend-owned D-Bus drain before actor admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendEvent {
    /// Cache add/remove/invalidation input.
    Cache(CacheEvent),
    /// A non-cache object signal useful to higher layers.
    ObjectChanged {
        /// Source address when the signal header supplied one.
        source: Option<ObjectAddress>,
        /// Bounded normalized event kind.
        kind: String,
    },
    /// A validated object signal requiring one coalesced actor-owned refresh.
    RefreshObject {
        /// Exact unique sender/path from the signal header.
        source: ObjectAddress,
        /// Bounded normalized event kind.
        kind: String,
    },
    /// One application could not supply a coherent bulk cache snapshot.
    ApplicationDegraded {
        /// Unique application bus name.
        bus_name: String,
        /// Static degradation category.
        reason: &'static str,
    },
    /// The backend detected an ordering gap or unsupported signature.
    ResyncRequired {
        /// Static reason suitable for metrics and logs.
        reason: &'static str,
    },
    /// The signal stream returned an error and stopped.
    StreamFailed(BackendFailure),
    /// The underlying zbus connection closed.
    ConnectionClosed,
}

/// Result of a nonblocking event admission attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventOfferResult {
    /// Event entered the bounded queue.
    Accepted,
    /// Queue was full; a capacity-independent resync epoch was advanced.
    Overflowed,
    /// Actor/receiver is gone.
    Closed,
}

/// Cloneable nonblocking ingress passed to a dedicated signal-drain task.
#[derive(Clone, Debug)]
pub struct BackendEventIngress {
    sender: mpsc::Sender<BackendEvent>,
    overflow: watch::Sender<u64>,
    change_epoch: Arc<AtomicU64>,
}

impl BackendEventIngress {
    /// Offer an event without awaiting a potentially saturated actor channel.
    pub fn offer(&self, event: BackendEvent) -> EventOfferResult {
        let epoch = self.change_epoch.load(Ordering::SeqCst);
        if epoch > u64::MAX - 2 {
            self.change_epoch.store(u64::MAX, Ordering::SeqCst);
            self.signal_overflow();
            return EventOfferResult::Overflowed;
        }
        if epoch & 1 != 0
            || self
                .change_epoch
                .compare_exchange(epoch, epoch + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            self.signal_overflow();
            return EventOfferResult::Overflowed;
        }
        let result = match self.sender.try_send(event) {
            Ok(()) => EventOfferResult::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.signal_overflow();
                EventOfferResult::Overflowed
            }
            Err(mpsc::error::TrySendError::Closed(_)) => EventOfferResult::Closed,
        };
        self.change_epoch.store(epoch + 2, Ordering::SeqCst);
        result
    }

    fn signal_overflow(&self) {
        let current = *self.overflow.borrow();
        self.overflow.send_replace(current.saturating_add(1));
    }
}

/// Backend instance exclusively owned by one actor connection generation.
pub trait AtspiBackend: Send + 'static {
    /// Load a bounded coherent cache snapshot. The actor wraps this in its whole-operation timeout.
    fn bootstrap(
        &mut self,
        limits: CacheLimits,
        proxy_call_timeout: Duration,
    ) -> BackendFuture<'_, Result<Vec<crate::cache::NormalizedCacheItem>, BackendFailure>>;

    /// Execute one already revalidated semantic operation on the central bus.
    ///
    /// Implementations must call `dispatch.mark_dispatched()` immediately
    /// before the mutating bus method. The default keeps portable fake and
    /// disabled backends fail-closed.
    fn execute_semantic(
        &mut self,
        _request: BackendSemanticRequest,
        _dispatch: SemanticDispatchMarker,
    ) -> BackendFuture<'_, Result<crate::semantic::SemanticEvidence, BackendFailure>> {
        Box::pin(async {
            Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "semantic operations are unsupported by this backend",
            ))
        })
    }

    /// Read fresh exact object/application evidence without exposing a bus proxy.
    fn observe_exact(
        &mut self,
        _request: BackendObservationRequest,
    ) -> BackendFuture<'_, Result<crate::semantic::SemanticObservationEvidence, BackendFailure>>
    {
        Box::pin(async {
            Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "fresh semantic observations are unsupported by this backend",
            ))
        })
    }

    /// Refresh one known cache object without exposing a bus proxy.
    fn refresh_object(
        &mut self,
        _request: BackendRefreshRequest,
    ) -> BackendFuture<'_, Result<RefreshedCacheItem, BackendFailure>> {
        Box::pin(async {
            Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "targeted object refresh is unsupported by this backend",
            ))
        })
    }

    /// Stop child drains and release bus resources. The actor externally bounds this future.
    fn shutdown(&mut self) -> BackendFuture<'_, ()>;
}

/// Actor-validated input for one bounded targeted cache refresh.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendRefreshRequest {
    /// Exact source from a validated object signal.
    pub object: ObjectAddress,
    /// Application root already proven by the actor cache.
    pub expected_application: ObjectAddress,
    /// Whole refresh deadline.
    pub timeout: Duration,
    /// Cache admission limits applied before returning the item.
    pub cache_limits: CacheLimits,
}

/// Factory for actor-owned backend generations.
pub trait AtspiBackendConnector: Send + 'static {
    /// Concrete backend that never escapes the actor task.
    type Backend: AtspiBackend;

    /// Establish one new generation and attach its signal drain to `ingress`.
    fn connect(
        &mut self,
        ingress: BackendEventIngress,
        cache_limits: CacheLimits,
    ) -> BackendFuture<'_, Result<Self::Backend, BackendFailure>>;
}

/// Normalized event emitted by the actor without exposing zbus types or proxies.
#[derive(Clone, Debug, PartialEq)]
pub enum AtspiActorEvent {
    /// Independent semantic health changed.
    HealthChanged(AtspiActorHealth),
    /// The bounded cache revision changed.
    CacheChanged {
        /// Accessibility connection generation that owns this mutation.
        accessibility_generation: u64,
        /// Immediately preceding cache revision required by an incremental mirror.
        previous_revision: u64,
        /// Current monotonic revision.
        revision: u64,
        /// Bounded exact mutation detail.
        mutation: CacheMutationDetail,
        /// Current bounded node count.
        cached_nodes: usize,
        /// Estimated normalized bytes retained after the mutation.
        cached_bytes: usize,
    },
    /// One application owner disappeared and its references became stale.
    ApplicationInvalidated {
        /// Accessibility connection generation that owns this invalidation.
        accessibility_generation: u64,
        /// Cache revision containing the invalidation.
        cache_revision: u64,
        /// Unique bus name of the invalidated application.
        bus_name: String,
        /// New application instance generation.
        application_generation: u64,
    },
    /// Non-cache object event forwarded without blocking the D-Bus drain.
    ObjectChanged {
        /// Accessibility generation containing the committed metadata state.
        accessibility_generation: u64,
        /// Cache revision committed before this event was emitted.
        cache_revision: u64,
        /// Source object when known.
        source: Option<ObjectAddress>,
        /// Normalized event kind.
        kind: String,
    },
    /// One application lacks a usable bulk cache and requires bounded lazy fallback.
    ApplicationDegraded {
        /// Unique application bus name.
        bus_name: String,
        /// Static degradation category.
        reason: &'static str,
    },
    /// Consumers must discard partial history and reacquire a snapshot.
    ResyncRequired {
        /// Accessibility generation for the rebuild.
        accessibility_generation: u64,
        /// Cache revision from which a new paginated snapshot must begin.
        cache_revision: u64,
        /// Static failure/overflow category.
        reason: &'static str,
    },
}

#[derive(Clone, Debug)]
struct ActorEventEmitter {
    sender: mpsc::Sender<AtspiActorEvent>,
    overflow: watch::Sender<u64>,
}

impl ActorEventEmitter {
    fn offer(&self, event: AtspiActorEvent) {
        if matches!(
            self.sender.try_send(event),
            Err(mpsc::error::TrySendError::Full(_))
        ) {
            let next = self.overflow.borrow().saturating_add(1);
            self.overflow.send_replace(next);
        }
    }
}

/// Single-consumer normalized event receiver with explicit overflow recovery.
#[derive(Debug)]
pub struct AtspiEventReceiver {
    receiver: mpsc::Receiver<AtspiActorEvent>,
    overflow: watch::Receiver<u64>,
    generation: watch::Receiver<AtspiActorHealth>,
}

/// Result of one nonblocking normalized-event receive attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum AtspiTryRecv {
    /// One ordered event, including a synthesized overflow barrier.
    Event(AtspiActorEvent),
    /// No event is currently ready and the stream remains open.
    Empty,
    /// The actor event stream is closed and no overflow barrier remains.
    Closed,
}

impl AtspiEventReceiver {
    /// Return the current monotonic public-event overflow epoch.
    #[must_use]
    pub fn overflow_epoch(&self) -> u64 {
        *self.overflow.borrow()
    }

    /// Try to receive without waiting, preserving overflow-before-queue semantics.
    pub fn try_recv(&mut self) -> AtspiTryRecv {
        if self.consume_overflow() {
            return AtspiTryRecv::Event(self.overflow_event());
        }
        let queued = self.receiver.try_recv();
        if self.consume_overflow() {
            return AtspiTryRecv::Event(self.overflow_event());
        }
        match queued {
            Ok(event) => AtspiTryRecv::Event(event),
            Err(mpsc::error::TryRecvError::Empty) => AtspiTryRecv::Empty,
            Err(mpsc::error::TryRecvError::Disconnected) => AtspiTryRecv::Closed,
        }
    }

    /// Receive the next event, prioritizing a latched overflow resync over stale queued data.
    pub async fn recv(&mut self) -> Option<AtspiActorEvent> {
        if self.overflow.has_changed().unwrap_or(false) {
            self.overflow.borrow_and_update();
            self.discard_ambiguous_queue();
            return Some(self.overflow_event());
        }
        tokio::select! {
            biased;
            changed = self.overflow.changed() => {
                if changed.is_err() {
                    self.receiver.recv().await
                } else {
                    self.overflow.borrow_and_update();
                    self.discard_ambiguous_queue();
                    Some(self.overflow_event())
                }
            }
            event = self.receiver.recv() => event,
        }
    }

    fn overflow_event(&self) -> AtspiActorEvent {
        let health = self.generation.borrow().clone();
        AtspiActorEvent::ResyncRequired {
            accessibility_generation: health.accessibility_generation,
            cache_revision: health.cache_revision,
            reason: "public_event_queue_overflow",
        }
    }

    fn discard_ambiguous_queue(&mut self) {
        while self.receiver.try_recv().is_ok() {}
    }

    fn consume_overflow(&mut self) -> bool {
        if self.overflow.has_changed().unwrap_or(false) {
            self.overflow.borrow_and_update();
            self.discard_ambiguous_queue();
            true
        } else {
            false
        }
    }
}

/// Bounded read-only actor snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtspiActorSnapshot {
    /// Current independent health and generation evidence.
    pub health: AtspiActorHealth,
}

/// Cloneable typed handle; it owns no D-Bus connection or proxy.
#[derive(Clone, Debug)]
pub struct AtspiHandle {
    requests: mpsc::Sender<ActorRequest>,
    health: watch::Receiver<AtspiActorHealth>,
    shutdown: CancellationToken,
}

impl AtspiHandle {
    /// Return the latest watch snapshot without queue admission.
    #[must_use]
    pub fn health(&self) -> AtspiActorHealth {
        self.health.borrow().clone()
    }

    /// Request a serialized snapshot from the owner task.
    pub async fn snapshot(
        &self,
        cancellation: CancellationToken,
    ) -> Result<AtspiActorSnapshot, AtspiActorError> {
        let (reply, receiver) = oneshot::channel();
        self.submit(ActorRequest::Snapshot {
            cancellation: cancellation.clone(),
            reply,
        })?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(AtspiActorError::Cancelled),
            () = self.shutdown.cancelled() => Err(AtspiActorError::Stopped),
            result = receiver => result.unwrap_or(Err(AtspiActorError::Stopped)),
        }
    }

    /// Force invalidation and a bounded reconnect/bootstrap cycle.
    pub async fn rebuild(&self, cancellation: CancellationToken) -> Result<(), AtspiActorError> {
        let (reply, receiver) = oneshot::channel();
        self.submit(ActorRequest::Rebuild {
            cancellation: cancellation.clone(),
            reply,
        })?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(AtspiActorError::Cancelled),
            () = self.shutdown.cancelled() => Err(AtspiActorError::Stopped),
            result = receiver => result.unwrap_or(Err(AtspiActorError::Stopped)),
        }
    }

    /// Read one deterministic actor-owned cache page without exposing bus proxies.
    pub async fn cache_page(
        &self,
        expected_accessibility_generation: Option<u64>,
        expected_revision: Option<u64>,
        after: Option<ObjectAddress>,
        cancellation: CancellationToken,
    ) -> Result<CachePage, AtspiActorError> {
        let (reply, receiver) = oneshot::channel();
        self.submit(ActorRequest::CachePage {
            expected_accessibility_generation,
            expected_revision,
            after,
            cancellation: cancellation.clone(),
            reply,
        })?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(AtspiActorError::Cancelled),
            () = self.shutdown.cancelled() => Err(AtspiActorError::Stopped),
            result = receiver => result.unwrap_or(Err(AtspiActorError::Stopped)),
        }
    }

    /// Execute one serialized, generation-fenced semantic operation.
    pub async fn execute_semantic(
        &self,
        request: SemanticRequest,
        cancellation: CancellationToken,
    ) -> Result<SemanticResult, SemanticError> {
        let (reply, receiver) = oneshot::channel();
        match self.requests.try_send(ActorRequest::Semantic {
            request,
            cancellation,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        receiver
            .await
            .unwrap_or(Err(SemanticError::ReplyLostAfterAdmission))
    }

    /// Mint an exact semantic target from actor-owned cache identity evidence.
    pub async fn semantic_target(
        &self,
        request: SemanticTargetRequest,
        cancellation: CancellationToken,
    ) -> Result<SemanticTarget, SemanticError> {
        let (reply, receiver) = oneshot::channel();
        match self.requests.try_send(ActorRequest::SemanticTarget {
            request,
            cancellation,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        receiver.await.unwrap_or(Err(SemanticError::Stopped))
    }

    /// Mint one exact target for a non-Tokio queue-head precondition.
    ///
    /// The actor still owns all cache identity fields. The blocking caller
    /// supplies only a daemon-mirrored coordinate and waits within an explicit
    /// bound; timeout synchronously cancels the admitted request.
    pub fn semantic_target_blocking(
        &self,
        request: SemanticTargetRequest,
        timeout: Duration,
    ) -> Result<SemanticTarget, SemanticError> {
        if timeout.is_zero() {
            return Err(SemanticError::InvalidRequest(
                "semantic target blocking timeout is zero",
            ));
        }
        let cancellation = CancellationToken::new();
        let (reply, receiver) = std_mpsc::sync_channel(1);
        match self
            .requests
            .try_send(ActorRequest::SemanticTargetBlocking {
                request,
                cancellation: cancellation.clone(),
                reply,
            }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                cancellation.cancel();
                Err(SemanticError::DeadlineBeforeDispatch)
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(SemanticError::Stopped),
        }
    }

    /// Fresh-read and reconcile one exact target through the actor-owned backend.
    pub async fn reconcile_semantic_target(
        &self,
        request: SemanticTargetRequest,
        deadline: tokio::time::Instant,
        cancellation: CancellationToken,
    ) -> Result<SemanticReconcileResult, SemanticError> {
        let (reply, receiver) = oneshot::channel();
        match self
            .requests
            .try_send(ActorRequest::ReconcileSemanticTarget {
                request,
                deadline,
                cancellation,
                reply,
            }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        receiver.await.unwrap_or(Err(SemanticError::Stopped))
    }

    /// Read one serialized fresh exact observation without exposing a backend proxy.
    pub async fn observe_semantic(
        &self,
        request: SemanticObservationRequest,
        cancellation: CancellationToken,
    ) -> Result<SemanticObservationResult, SemanticError> {
        let (reply, receiver) = oneshot::channel();
        match self.requests.try_send(ActorRequest::Observe {
            request,
            cancellation,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        receiver.await.unwrap_or(Err(SemanticError::Stopped))
    }

    /// Queue an exact owner-task observation and synchronously await its bounded reply.
    ///
    /// This is intended for a non-Tokio input actor's queue-head precondition.
    /// It never owns or escapes a D-Bus proxy and never invokes a runtime `block_on`.
    pub fn observe_semantic_blocking(
        &self,
        mut request: SemanticObservationRequest,
        timeout: Duration,
    ) -> Result<SemanticObservationResult, SemanticError> {
        if timeout.is_zero() {
            return Err(SemanticError::InvalidRequest(
                "semantic observation blocking timeout is zero",
            ));
        }
        let timeout_deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or(request.deadline);
        request.deadline = request.deadline.min(timeout_deadline);
        let cancellation = CancellationToken::new();
        let (reply, receiver) = std_mpsc::sync_channel(1);
        match self.requests.try_send(ActorRequest::ObserveBlocking {
            request,
            cancellation: cancellation.clone(),
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SemanticError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SemanticError::Stopped),
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                cancellation.cancel();
                Err(SemanticError::DeadlineBeforeDispatch)
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                cancellation.cancel();
                Err(SemanticError::Stopped)
            }
        }
    }

    fn submit(&self, request: ActorRequest) -> Result<(), AtspiActorError> {
        match self.requests.try_send(request) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(AtspiActorError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(AtspiActorError::Stopped),
        }
    }
}

/// Join owner and explicit cancellation for the actor task set.
#[derive(Debug)]
pub struct AtspiActorJoin {
    shutdown: CancellationToken,
    join: Option<JoinHandle<AtspiActorExit>>,
}

impl AtspiActorJoin {
    /// Request shutdown and wait until the backend/drain cleanup deadline completes.
    pub async fn shutdown(mut self) -> AtspiActorExit {
        self.shutdown.cancel();
        self.wait_inner().await
    }

    /// Wait for actor termination without initiating it.
    pub async fn wait(mut self) -> AtspiActorExit {
        self.wait_inner().await
    }

    async fn wait_inner(&mut self) -> AtspiActorExit {
        let Some(join) = self.join.take() else {
            return AtspiActorExit::Stopped;
        };
        match join.await {
            Ok(exit) => exit,
            Err(_) => AtspiActorExit::Panicked,
        }
    }
}

impl Drop for AtspiActorJoin {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Terminal actor outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtspiActorExit {
    /// Explicit shutdown or disabled actor termination.
    Stopped,
    /// Actor task panicked.
    Panicked,
}

/// Fully owned result of spawning an AT-SPI actor.
#[derive(Debug)]
pub struct SpawnedAtspiActor {
    /// Typed request/health handle.
    pub handle: AtspiHandle,
    /// Single-consumer normalized event stream.
    pub events: AtspiEventReceiver,
    /// Task-set join and shutdown owner.
    pub join: AtspiActorJoin,
}

/// Validate configuration and start one single-owner actor task set.
pub fn spawn_atspi_actor<C>(
    enabled: bool,
    config: AtspiActorConfig,
    connector: C,
) -> Result<SpawnedAtspiActor, AtspiActorSpawnError>
where
    C: AtspiBackendConnector,
{
    let config = config.validate()?;
    let (request_sender, request_receiver) = mpsc::channel(config.request_capacity);
    let initial_health = AtspiActorHealth::initial(enabled);
    let (health_sender, health_receiver) = watch::channel(initial_health);
    let event_health = health_receiver.clone();
    let (event_sender, event_receiver) = mpsc::channel(config.event_capacity);
    let (overflow_sender, overflow_receiver) = watch::channel(0_u64);
    let emitter = ActorEventEmitter {
        sender: event_sender,
        overflow: overflow_sender,
    };
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let supervisor_health = health_sender.clone();
    let supervisor_emitter = emitter.clone();
    let actor = tokio::spawn(run_actor(
        enabled,
        config,
        connector,
        request_receiver,
        health_sender,
        emitter,
        task_shutdown,
    ));
    let supervisor = tokio::spawn(async move {
        match actor.await {
            Ok(exit) => exit,
            Err(_) => {
                let mut health = supervisor_health.borrow().clone();
                health.state = AtspiActorState::Panicked;
                health.last_error = Some("AT-SPI actor panicked".to_owned());
                supervisor_health.send_replace(health.clone());
                supervisor_emitter.offer(AtspiActorEvent::HealthChanged(health));
                AtspiActorExit::Panicked
            }
        }
    });
    Ok(SpawnedAtspiActor {
        handle: AtspiHandle {
            requests: request_sender,
            health: health_receiver,
            shutdown: shutdown.clone(),
        },
        events: AtspiEventReceiver {
            receiver: event_receiver,
            overflow: overflow_receiver,
            generation: event_health,
        },
        join: AtspiActorJoin {
            shutdown,
            join: Some(supervisor),
        },
    })
}

#[derive(Debug)]
enum ActorRequest {
    Snapshot {
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<AtspiActorSnapshot, AtspiActorError>>,
    },
    Rebuild {
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<(), AtspiActorError>>,
    },
    CachePage {
        expected_accessibility_generation: Option<u64>,
        expected_revision: Option<u64>,
        after: Option<ObjectAddress>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<CachePage, AtspiActorError>>,
    },
    Semantic {
        request: SemanticRequest,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<SemanticResult, SemanticError>>,
    },
    SemanticTarget {
        request: SemanticTargetRequest,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<SemanticTarget, SemanticError>>,
    },
    SemanticTargetBlocking {
        request: SemanticTargetRequest,
        cancellation: CancellationToken,
        reply: std_mpsc::SyncSender<Result<SemanticTarget, SemanticError>>,
    },
    ReconcileSemanticTarget {
        request: SemanticTargetRequest,
        deadline: tokio::time::Instant,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<SemanticReconcileResult, SemanticError>>,
    },
    Observe {
        request: SemanticObservationRequest,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<SemanticObservationResult, SemanticError>>,
    },
    ObserveBlocking {
        request: SemanticObservationRequest,
        cancellation: CancellationToken,
        reply: std_mpsc::SyncSender<Result<SemanticObservationResult, SemanticError>>,
    },
}

async fn run_actor<C>(
    enabled: bool,
    config: AtspiActorConfig,
    mut connector: C,
    mut requests: mpsc::Receiver<ActorRequest>,
    health_sender: watch::Sender<AtspiActorHealth>,
    emitter: ActorEventEmitter,
    shutdown: CancellationToken,
) -> AtspiActorExit
where
    C: AtspiBackendConnector,
{
    let mut cache = match BoundedCache::new(config.cache_limits) {
        Ok(cache) => cache,
        Err(error) => {
            let mut health = health_sender.borrow().clone();
            health.state = AtspiActorState::Panicked;
            health.last_error = Some(error.to_string());
            publish_health(&health_sender, &emitter, health);
            return AtspiActorExit::Panicked;
        }
    };
    if !enabled {
        run_disabled(&mut requests, &health_sender, &shutdown).await;
        stop_health(&health_sender, &emitter, &cache);
        return AtspiActorExit::Stopped;
    }

    let mut established = false;
    let mut reconnect_delay = config.reconnect_initial;
    let mut semantic_read_epoch = 0_u64;
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let state = if established {
            AtspiActorState::Reconnecting
        } else {
            AtspiActorState::Connecting
        };
        let last_error = health_sender.borrow().last_error.clone();
        update_health(&health_sender, &emitter, state, &cache, last_error);
        let (backend_sender, mut backend_receiver) = mpsc::channel(config.backend_event_capacity);
        let (backend_overflow_sender, mut backend_overflow_receiver) = watch::channel(0_u64);
        let backend_change_epoch = Arc::new(AtomicU64::new(0));
        let ingress = BackendEventIngress {
            sender: backend_sender,
            overflow: backend_overflow_sender,
            change_epoch: Arc::clone(&backend_change_epoch),
        };
        let backend = {
            let connection = connector.connect(ingress, config.cache_limits);
            let mut connection = Box::pin(timeout(config.connect_timeout, connection));
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break None,
                    request = requests.recv() => {
                        match request {
                            Some(request) => respond_transitional(request, &health_sender),
                            None => {
                                shutdown.cancel();
                                break None;
                            }
                        }
                    }
                    result = &mut connection => {
                        let result = match result {
                            Ok(value) => value,
                            Err(_) => Err(BackendFailure::new(BackendFailureKind::Timeout, "connection deadline exceeded")),
                        };
                        break Some(result);
                    }
                }
            }
        };
        let mut backend = match backend {
            None => break,
            Some(Ok(backend)) => backend,
            Some(Err(error)) => {
                update_health(
                    &health_sender,
                    &emitter,
                    AtspiActorState::Reconnecting,
                    &cache,
                    Some(error.to_string()),
                );
                if wait_reconnect_delay(reconnect_delay, &mut requests, &health_sender, &shutdown)
                    .await
                {
                    break;
                }
                reconnect_delay = next_delay(reconnect_delay, config.reconnect_max);
                continue;
            }
        };

        let bootstrap_result = {
            let bootstrap = backend.bootstrap(config.cache_limits, config.proxy_call_timeout);
            let mut bootstrap = Box::pin(timeout(config.bootstrap_timeout, bootstrap));
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break None,
                    request = requests.recv() => {
                        match request {
                            Some(request) => respond_transitional(request, &health_sender),
                            None => {
                                shutdown.cancel();
                                break None;
                            }
                        }
                    }
                    result = &mut bootstrap => break Some(match result {
                        Ok(value) => value,
                        Err(_) => Err(BackendFailure::new(BackendFailureKind::Timeout, "cache bootstrap deadline exceeded")),
                    }),
                }
            }
        };
        if shutdown.is_cancelled() {
            let _ = timeout(config.shutdown_timeout, backend.shutdown()).await;
            drop(backend);
            break;
        }
        let bootstrap_result = match bootstrap_result {
            Some(Ok(items)) => cache.replace(items).map_err(|error| error.to_string()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err(CacheError::Malformed("bootstrap did not complete").to_string()),
        };
        if let Err(error) = bootstrap_result {
            established = true;
            if !invalidate_generation(
                &health_sender,
                &emitter,
                &mut cache,
                "cache_bootstrap_failed",
                Some(error),
            ) {
                break;
            }
            let _ = timeout(config.shutdown_timeout, backend.shutdown()).await;
            drop(backend);
            if shutdown.is_cancelled()
                || wait_reconnect_delay(reconnect_delay, &mut requests, &health_sender, &shutdown)
                    .await
            {
                break;
            }
            reconnect_delay = next_delay(reconnect_delay, config.reconnect_max);
            continue;
        }

        established = true;
        reconnect_delay = config.reconnect_initial;
        update_health(
            &health_sender,
            &emitter,
            AtspiActorState::Healthy,
            &cache,
            None,
        );
        let mut pending_refreshes = BTreeMap::<ObjectAddress, BTreeSet<String>>::new();
        let disconnect_reason = 'connected: loop {
            if backend_overflow_receiver.has_changed().unwrap_or(false) {
                backend_overflow_receiver.borrow_and_update();
                break "backend_event_queue_overflow";
            }
            // Drain at most the fixed queue capacity before admitting a public
            // request. This makes already-enqueued cache invalidations win over
            // semantic effects without allowing an unbounded signal flood to
            // starve the request queue.
            for _ in 0..config.backend_event_capacity {
                match backend_receiver.try_recv() {
                    Ok(event) => {
                        if let Some(reason) = process_backend_event(
                            event,
                            &mut cache,
                            &health_sender,
                            &emitter,
                            config,
                            &mut pending_refreshes,
                        ) {
                            break 'connected reason;
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        break 'connected "backend_connection_closed";
                    }
                }
            }
            if let Some(reason) = reconcile_dirty_objects(
                &mut backend,
                &mut cache,
                &health_sender,
                &emitter,
                config,
                &mut pending_refreshes,
            )
            .await
            {
                break reason;
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break "shutdown",
                changed = backend_overflow_receiver.changed() => {
                    if changed.is_ok() {
                        backend_overflow_receiver.borrow_and_update();
                        break "backend_event_queue_overflow";
                    }
                }
                request = requests.recv() => {
                    // Close the select wake-up race: cache signals may have
                    // arrived after the top-of-loop drain but before this
                    // request branch won. Reconcile the bounded queue again
                    // before any public operation observes or mutates state.
                    let mut admitted_epoch = None;
                    for _ in 0..=config.backend_event_capacity {
                        let candidate_epoch = backend_change_epoch.load(Ordering::SeqCst);
                        match backend_receiver.try_recv() {
                            Ok(event) => {
                                if let Some(reason) = process_backend_event(
                                    event,
                                    &mut cache,
                                    &health_sender,
                                    &emitter,
                                    config,
                                    &mut pending_refreshes,
                                ) {
                                    break 'connected reason;
                                }
                            }
                            Err(mpsc::error::TryRecvError::Empty) => {
                                let confirmed_epoch =
                                    backend_change_epoch.load(Ordering::SeqCst);
                                if stable_empty_epoch(candidate_epoch, confirmed_epoch) {
                                    admitted_epoch = Some(confirmed_epoch);
                                    break;
                                }
                            }
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                break 'connected "backend_connection_closed";
                            }
                        }
                    }
                    if admitted_epoch.is_none() {
                        break 'connected "backend_event_admission_race_limit";
                    }
                    if let Some(reason) = reconcile_dirty_objects(
                        &mut backend,
                        &mut cache,
                        &health_sender,
                        &emitter,
                        config,
                        &mut pending_refreshes,
                    )
                    .await
                    {
                        break 'connected reason;
                    }
                    match request {
                        Some(ActorRequest::Snapshot { cancellation, reply }) => {
                            let result = if cancellation.is_cancelled() {
                                Err(AtspiActorError::Cancelled)
                            } else {
                                Ok(AtspiActorSnapshot { health: health_sender.borrow().clone() })
                            };
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::Rebuild { cancellation, reply }) => {
                            if cancellation.is_cancelled() {
                                let _ignored = reply.send(Err(AtspiActorError::Cancelled));
                                continue;
                            }
                            let _ignored = reply.send(Ok(()));
                            break "requested_rebuild";
                        }
                        Some(ActorRequest::CachePage {
                            expected_accessibility_generation,
                            expected_revision,
                            after,
                            cancellation,
                            reply,
                        }) => {
                            let accessibility_generation =
                                health_sender.borrow().accessibility_generation;
                            let result = if cancellation.is_cancelled() {
                                Err(AtspiActorError::Cancelled)
                            } else if after.is_some()
                                && (expected_accessibility_generation.is_none()
                                    || expected_revision.is_none())
                            {
                                Err(AtspiActorError::InvalidPage)
                            } else if let Some(expected) = expected_accessibility_generation
                                && expected != accessibility_generation
                            {
                                Err(AtspiActorError::StaleGeneration {
                                    expected,
                                    current: accessibility_generation,
                                })
                            } else if let Some(expected) = expected_revision
                                && expected != cache.revision()
                            {
                                Err(AtspiActorError::StaleRevision {
                                    expected,
                                    current: cache.revision(),
                                })
                            } else {
                                cache
                                    .page(
                                        accessibility_generation,
                                        after.as_ref(),
                                        config.read_page_nodes,
                                        config.read_page_bytes,
                                    )
                                    .map(|mut page| {
                                        page.event_overflow_epoch = *emitter.overflow.borrow();
                                        page
                                    })
                                    .map_err(|_| AtspiActorError::InvalidPage)
                            };
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::Semantic {
                            request,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = execute_semantic_request(
                                &mut backend,
                                &cache,
                                &health,
                                config.proxy_call_timeout,
                                SemanticDispatchPermit::new(
                                    Arc::clone(&backend_change_epoch),
                                    admitted_epoch.unwrap_or(u64::MAX),
                                ),
                                request,
                                cancellation,
                            )
                            .await;
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::SemanticTarget {
                            request,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = materialize_semantic_target(
                                &cache,
                                &health,
                                request,
                                &cancellation,
                            );
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::SemanticTargetBlocking {
                            request,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = materialize_semantic_target(
                                &cache,
                                &health,
                                request,
                                &cancellation,
                            );
                            let _ignored = reply.try_send(result);
                        }
                        Some(ActorRequest::ReconcileSemanticTarget {
                            request,
                            deadline,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = reconcile_semantic_target_request(
                                &mut backend,
                                &mut cache,
                                &health,
                                &health_sender,
                                &emitter,
                                config,
                                SemanticDispatchPermit::new(
                                    Arc::clone(&backend_change_epoch),
                                    admitted_epoch.unwrap_or(u64::MAX),
                                ),
                                request,
                                deadline,
                                cancellation,
                            )
                            .await;
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::Observe {
                            request,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = execute_observation_request(
                                &mut backend,
                                &cache,
                                &health,
                                config,
                                SemanticDispatchPermit::new(
                                    Arc::clone(&backend_change_epoch),
                                    admitted_epoch.unwrap_or(u64::MAX),
                                ),
                                &mut semantic_read_epoch,
                                request,
                                cancellation,
                            )
                            .await;
                            let _ignored = reply.send(result);
                        }
                        Some(ActorRequest::ObserveBlocking {
                            request,
                            cancellation,
                            reply,
                        }) => {
                            let health = health_sender.borrow().clone();
                            let result = execute_observation_request(
                                &mut backend,
                                &cache,
                                &health,
                                config,
                                SemanticDispatchPermit::new(
                                    Arc::clone(&backend_change_epoch),
                                    admitted_epoch.unwrap_or(u64::MAX),
                                ),
                                &mut semantic_read_epoch,
                                request,
                                cancellation,
                            )
                            .await;
                            let _ignored = reply.try_send(result);
                        }
                        None => {
                            shutdown.cancel();
                            break "shutdown";
                        }
                    }
                }
                event = backend_receiver.recv() => {
                    match event {
                        Some(event) => {
                            if let Some(reason) = process_backend_event(
                                event,
                                &mut cache,
                                &health_sender,
                                &emitter,
                                config,
                                &mut pending_refreshes,
                            ) {
                                break reason;
                            }
                        }
                        None => break "backend_connection_closed",
                    }
                }
            }
        };
        let _ = timeout(config.shutdown_timeout, backend.shutdown()).await;
        drop(backend);
        if disconnect_reason == "shutdown" || shutdown.is_cancelled() {
            break;
        }
        if !invalidate_generation(
            &health_sender,
            &emitter,
            &mut cache,
            disconnect_reason,
            Some(disconnect_reason.to_owned()),
        ) {
            break;
        }
        if wait_reconnect_delay(reconnect_delay, &mut requests, &health_sender, &shutdown).await {
            break;
        }
        reconnect_delay = next_delay(reconnect_delay, config.reconnect_max);
    }
    stop_health(&health_sender, &emitter, &cache);
    AtspiActorExit::Stopped
}

fn process_backend_event(
    event: BackendEvent,
    cache: &mut BoundedCache,
    health_sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    config: AtspiActorConfig,
    pending_refreshes: &mut BTreeMap<ObjectAddress, BTreeSet<String>>,
) -> Option<&'static str> {
    match event {
        BackendEvent::Cache(event) => {
            let protocol_gap = matches!(&event, CacheEvent::ProtocolGap);
            let invalidated_bus = match &event {
                CacheEvent::InvalidateApplication(bus) => Some(bus.clone()),
                _ => None,
            };
            let mutation = match cache.apply(event) {
                Ok(mutation) => mutation,
                Err(_) => return Some("cache_incremental_limit_or_protocol_error"),
            };
            if protocol_gap {
                return Some("backend_protocol_gap");
            }
            let Some(previous_revision) = mutation.revision.checked_sub(1) else {
                return Some("cache_revision_underflow");
            };
            refresh_cache_health(health_sender, cache);
            let accessibility_generation = health_sender.borrow().accessibility_generation;
            if mutation.kind == CacheMutationKind::ResyncRequired {
                emitter.offer(AtspiActorEvent::ResyncRequired {
                    accessibility_generation,
                    cache_revision: mutation.revision,
                    reason: "cache_mutation_detail_unavailable",
                });
                return Some("cache_mutation_detail_unavailable");
            }
            if mutation.kind != CacheMutationKind::Unchanged {
                emitter.offer(AtspiActorEvent::CacheChanged {
                    accessibility_generation,
                    previous_revision,
                    revision: mutation.revision,
                    mutation: mutation.detail,
                    cached_nodes: cache.len(),
                    cached_bytes: cache.bytes(),
                });
                if mutation.kind == CacheMutationKind::ApplicationInvalidated
                    && let Some(bus_name) = invalidated_bus
                {
                    emitter.offer(AtspiActorEvent::ApplicationInvalidated {
                        accessibility_generation,
                        cache_revision: mutation.revision,
                        application_generation: cache.application_generation(&bus_name),
                        bus_name,
                    });
                }
            }
            None
        }
        BackendEvent::ObjectChanged { source, kind } => {
            if kind.len() > config.cache_limits.max_string_bytes {
                return Some("backend_event_kind_limit_exceeded");
            }
            let health = health_sender.borrow();
            emitter.offer(AtspiActorEvent::ObjectChanged {
                accessibility_generation: health.accessibility_generation,
                cache_revision: cache.revision(),
                source,
                kind,
            });
            None
        }
        BackendEvent::RefreshObject { source, kind } => {
            if kind.len() > config.cache_limits.max_string_bytes {
                return Some("backend_event_kind_limit_exceeded");
            }
            let kinds = pending_refreshes.entry(source).or_default();
            kinds.insert(kind);
            let pending_kinds = pending_refreshes.values().map(BTreeSet::len).sum::<usize>();
            if pending_kinds > config.backend_event_capacity {
                return Some("targeted_object_refresh_metadata_overflow");
            }
            None
        }
        BackendEvent::ApplicationDegraded { bus_name, reason } => {
            if bus_name.len() > crate::cache::MAX_BUS_NAME_BYTES || !bus_name.starts_with(':') {
                return Some("backend_application_identity_invalid");
            }
            emitter.offer(AtspiActorEvent::ApplicationDegraded { bus_name, reason });
            None
        }
        BackendEvent::ResyncRequired { reason } => Some(reason),
        BackendEvent::StreamFailed(_) => Some("backend_stream_failed"),
        BackendEvent::ConnectionClosed => Some("backend_connection_closed"),
    }
}

async fn reconcile_dirty_objects<B: AtspiBackend>(
    backend: &mut B,
    cache: &mut BoundedCache,
    health_sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    config: AtspiActorConfig,
    pending_refreshes: &mut BTreeMap<ObjectAddress, BTreeSet<String>>,
) -> Option<&'static str> {
    let pending = std::mem::take(pending_refreshes);
    for (object, kinds) in pending {
        let Some(node) = cache.get(&object) else {
            // Toolkits may emit state/focus notifications for transient or
            // degraded objects that were never part of the authoritative
            // Cache snapshot. They cannot identify a public object, are not
            // evidence that the cache is corrupt, and are suppressed so an
            // untracked application cannot flood the public event lane.
            continue;
        };
        let request = BackendRefreshRequest {
            object: object.clone(),
            expected_application: node.item.application.clone(),
            timeout: config.proxy_call_timeout,
            cache_limits: config.cache_limits,
        };
        let refreshed =
            match timeout(config.proxy_call_timeout, backend.refresh_object(request)).await {
                Ok(Ok(item)) => item,
                Ok(Err(_)) => return Some("targeted_object_refresh_failed"),
                Err(_) => return Some("targeted_object_refresh_timed_out"),
            };
        let mutation = match cache.refresh(refreshed) {
            Ok(mutation) => mutation,
            Err(_) => return Some("targeted_object_refresh_cache_limit_or_protocol_error"),
        };
        if mutation.kind != CacheMutationKind::Unchanged {
            let Some(previous_revision) = mutation.revision.checked_sub(1) else {
                return Some("cache_revision_underflow");
            };
            refresh_cache_health(health_sender, cache);
            emitter.offer(AtspiActorEvent::CacheChanged {
                accessibility_generation: health_sender.borrow().accessibility_generation,
                previous_revision,
                revision: mutation.revision,
                mutation: mutation.detail,
                cached_nodes: cache.len(),
                cached_bytes: cache.bytes(),
            });
        }
        for kind in kinds {
            emitter.offer(AtspiActorEvent::ObjectChanged {
                accessibility_generation: health_sender.borrow().accessibility_generation,
                cache_revision: cache.revision(),
                source: Some(object.clone()),
                kind,
            });
        }
    }
    None
}

async fn execute_semantic_request<B: AtspiBackend>(
    backend: &mut B,
    cache: &BoundedCache,
    health: &AtspiActorHealth,
    proxy_call_timeout: Duration,
    dispatch_permit: SemanticDispatchPermit,
    request: SemanticRequest,
    cancellation: CancellationToken,
) -> Result<SemanticResult, SemanticError> {
    if cancellation.is_cancelled() {
        return Err(SemanticError::CancelledBeforeDispatch);
    }
    if tokio::time::Instant::now() >= request.deadline {
        return Err(SemanticError::DeadlineBeforeDispatch);
    }
    request.operation.validate()?;
    validate_semantic_target(cache, health, &request)?;

    let SemanticRequest {
        target,
        operation,
        deadline,
    } = request;
    let expected_child_count = cache
        .get(&target.object)
        .and_then(|node| node.item.child_count);
    let dispatch = SemanticDispatchMarker::new();
    let backend_request = BackendSemanticRequest {
        object: target.object,
        application: target.application,
        expected_identity: target.identity_fingerprint,
        expected_index_in_parent: target.index_in_parent,
        expected_role: target.role,
        expected_states: target.states,
        expected_child_count,
        operation,
        deadline,
        proxy_call_timeout,
        cache_limits: cache.limits(),
        dispatch_permit,
    };
    let call = backend.execute_semantic(backend_request, dispatch.clone());
    tokio::pin!(call);
    let backend_result = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(if dispatch.was_dispatched() {
                SemanticError::CancelledAfterDispatch
            } else {
                SemanticError::CancelledBeforeDispatch
            });
        }
        () = sleep_until(deadline) => {
            return Err(if dispatch.was_dispatched() {
                SemanticError::DeadlineAfterDispatch
            } else {
                SemanticError::DeadlineBeforeDispatch
            });
        }
        result = &mut call => result,
    };
    let evidence = backend_result.map_err(|failure| {
        if dispatch.was_dispatched() {
            SemanticError::BackendAfterDispatch(failure)
        } else {
            match failure.kind {
                BackendFailureKind::ActionNotFound => SemanticError::ActionNotFound,
                BackendFailureKind::AmbiguousAction => SemanticError::AmbiguousAction,
                _ => SemanticError::Backend(failure),
            }
        }
    })?;
    Ok(SemanticResult {
        accessibility_generation: target.accessibility_generation,
        application_generation: target.application_generation,
        cache_revision: target.cache_revision,
        evidence,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the actor passes independent cache, health, deadline, permit, epoch, and cancellation fences"
)]
async fn execute_observation_request<B: AtspiBackend>(
    backend: &mut B,
    cache: &BoundedCache,
    health: &AtspiActorHealth,
    config: AtspiActorConfig,
    read_permit: SemanticDispatchPermit,
    read_epoch: &mut u64,
    request: SemanticObservationRequest,
    cancellation: CancellationToken,
) -> Result<SemanticObservationResult, SemanticError> {
    if cancellation.is_cancelled() {
        return Err(SemanticError::CancelledBeforeDispatch);
    }
    if tokio::time::Instant::now() >= request.deadline {
        return Err(SemanticError::DeadlineBeforeDispatch);
    }
    validate_observation_target(cache, health, &request)?;
    if health.state != AtspiActorState::Healthy {
        return Err(SemanticError::Unavailable);
    }
    let SemanticObservationRequest { target, deadline } = request;
    let backend_request = BackendObservationRequest {
        object: target.object.clone(),
        application: target.application.clone(),
        expected_identity: target.identity_fingerprint,
        expected_index_in_parent: target.index_in_parent,
        expected_role: target.role,
        proxy_call_timeout: config.proxy_call_timeout,
        cache_limits: config.cache_limits,
        read_permit,
    };
    let call = backend.observe_exact(backend_request);
    tokio::pin!(call);
    let evidence = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(SemanticError::CancelledBeforeDispatch),
        () = sleep_until(deadline) => return Err(SemanticError::DeadlineBeforeDispatch),
        result = &mut call => result.map_err(SemanticError::Backend)?,
    };
    let next_epoch = read_epoch
        .checked_add(1)
        .ok_or(SemanticError::ReadEpochExhausted)?;
    *read_epoch = next_epoch;
    Ok(SemanticObservationResult {
        accessibility_generation: target.accessibility_generation,
        application_generation: target.application_generation,
        // A fresh observation remains valid when unrelated cache entries
        // advanced after the target was minted. Report the actor's current
        // global revision while the exact target-node fence below proves this
        // object itself did not change.
        cache_revision: cache.revision(),
        read_epoch: next_epoch,
        object: target.object,
        application: target.application,
        evidence,
    })
}

fn validate_observation_target(
    cache: &BoundedCache,
    health: &AtspiActorHealth,
    request: &SemanticObservationRequest,
) -> Result<(), SemanticError> {
    let target = &request.target;
    if target.accessibility_generation != health.accessibility_generation {
        return Err(SemanticError::StaleAccessibilityGeneration {
            expected: target.accessibility_generation,
            current: health.accessibility_generation,
        });
    }
    if target.cache_revision > cache.revision() {
        return Err(SemanticError::StaleCacheRevision {
            expected: target.cache_revision,
            current: cache.revision(),
        });
    }
    let current_application_generation =
        cache.application_generation(target.application.bus_name());
    if target.application_generation != current_application_generation {
        return Err(SemanticError::StaleApplicationGeneration {
            expected: target.application_generation,
            current: current_application_generation,
        });
    }
    let node = cache
        .get(&target.object)
        .ok_or(SemanticError::StaleIdentity)?;
    if node.application_generation != target.application_generation
        || node.revision != target.node_revision
        || node.item.application != target.application
        || node.identity_fingerprint != target.identity_fingerprint
        || node.item.index_in_parent != target.index_in_parent
        || node.item.role != target.role
    {
        return Err(SemanticError::StaleIdentity);
    }
    Ok(())
}

fn materialize_semantic_target(
    cache: &BoundedCache,
    health: &AtspiActorHealth,
    request: SemanticTargetRequest,
    cancellation: &CancellationToken,
) -> Result<SemanticTarget, SemanticError> {
    if cancellation.is_cancelled() {
        return Err(SemanticError::CancelledBeforeDispatch);
    }
    if health.state != AtspiActorState::Healthy {
        return Err(SemanticError::Unavailable);
    }
    if request.accessibility_generation != health.accessibility_generation {
        return Err(SemanticError::StaleAccessibilityGeneration {
            expected: request.accessibility_generation,
            current: health.accessibility_generation,
        });
    }
    // A daemon mirror may lag the actor only because unrelated objects have
    // advanced. The exact application generation and node revision below are
    // the identity fences that matter for this target; a future mirror
    // revision remains impossible and fails closed.
    if request.cache_revision > cache.revision() {
        return Err(SemanticError::StaleCacheRevision {
            expected: request.cache_revision,
            current: cache.revision(),
        });
    }
    let current_application_generation =
        cache.application_generation(request.application.bus_name());
    if request.application_generation != current_application_generation {
        return Err(SemanticError::StaleApplicationGeneration {
            expected: request.application_generation,
            current: current_application_generation,
        });
    }
    let node = cache
        .get(&request.object)
        .ok_or(SemanticError::StaleIdentity)?;
    if node.application_generation != request.application_generation
        || node.revision != request.node_revision
        || node.item.application != request.application
    {
        return Err(SemanticError::StaleIdentity);
    }
    Ok(SemanticTarget {
        object: request.object,
        application: request.application,
        accessibility_generation: request.accessibility_generation,
        application_generation: request.application_generation,
        cache_revision: request.cache_revision,
        node_revision: request.node_revision,
        index_in_parent: node.item.index_in_parent,
        identity_fingerprint: node.identity_fingerprint.clone(),
        role: node.item.role,
        states: node.item.states.clone(),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the serialized reconcile path carries independent cache, health, event, deadline, and ingress fences"
)]
async fn reconcile_semantic_target_request<B: AtspiBackend>(
    backend: &mut B,
    cache: &mut BoundedCache,
    health: &AtspiActorHealth,
    health_sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    config: AtspiActorConfig,
    read_permit: SemanticDispatchPermit,
    request: SemanticTargetRequest,
    deadline: tokio::time::Instant,
    cancellation: CancellationToken,
) -> Result<SemanticReconcileResult, SemanticError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(SemanticError::DeadlineBeforeDispatch);
    }
    // Reconciliation may mutate the actor cache and publish an ordered mirror
    // delta, so unlike read-only target minting it must start from the exact
    // daemon-mirrored global revision.
    if request.cache_revision != cache.revision() {
        return Err(SemanticError::StaleCacheRevision {
            expected: request.cache_revision,
            current: cache.revision(),
        });
    }
    let target = materialize_semantic_target(cache, health, request, &cancellation)?;
    let backend_request = BackendRefreshRequest {
        object: target.object.clone(),
        expected_application: target.application.clone(),
        timeout: config.proxy_call_timeout,
        cache_limits: config.cache_limits,
    };
    let call = backend.refresh_object(backend_request);
    tokio::pin!(call);
    let refreshed = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(SemanticError::CancelledBeforeDispatch),
        () = sleep_until(deadline) => return Err(SemanticError::DeadlineBeforeDispatch),
        result = &mut call => result.map_err(SemanticError::Backend)?,
    };
    read_permit
        .ensure_current()
        .map_err(SemanticError::Backend)?;
    if refreshed.item.object != target.object || refreshed.item.application != target.application {
        return Err(SemanticError::Backend(BackendFailure::new(
            BackendFailureKind::Protocol,
            "targeted reconcile backend returned mismatched object provenance",
        )));
    }
    let previous_cache_revision = cache.revision();
    let mutation = cache.refresh(refreshed).map_err(|_error| {
        SemanticError::Backend(BackendFailure::new(
            BackendFailureKind::Protocol,
            "targeted reconcile cache refresh failed",
        ))
    })?;
    let changed = mutation.kind != CacheMutationKind::Unchanged;
    if changed {
        refresh_cache_health(health_sender, cache);
        emitter.offer(AtspiActorEvent::CacheChanged {
            accessibility_generation: target.accessibility_generation,
            previous_revision: previous_cache_revision,
            revision: mutation.revision,
            mutation: mutation.detail,
            cached_nodes: cache.len(),
            cached_bytes: cache.bytes(),
        });
    }
    let node_revision = cache
        .get(&target.object)
        .ok_or(SemanticError::StaleIdentity)?
        .revision;
    Ok(SemanticReconcileResult {
        accessibility_generation: target.accessibility_generation,
        application_generation: target.application_generation,
        previous_cache_revision,
        cache_revision: cache.revision(),
        node_revision,
        changed,
    })
}

fn validate_semantic_target(
    cache: &BoundedCache,
    health: &AtspiActorHealth,
    request: &SemanticRequest,
) -> Result<(), SemanticError> {
    let target = &request.target;
    if target.accessibility_generation != health.accessibility_generation {
        return Err(SemanticError::StaleAccessibilityGeneration {
            expected: target.accessibility_generation,
            current: health.accessibility_generation,
        });
    }
    if target.cache_revision != cache.revision() {
        return Err(SemanticError::StaleCacheRevision {
            expected: target.cache_revision,
            current: cache.revision(),
        });
    }
    let current_application_generation =
        cache.application_generation(target.application.bus_name());
    if target.application_generation != current_application_generation {
        return Err(SemanticError::StaleApplicationGeneration {
            expected: target.application_generation,
            current: current_application_generation,
        });
    }
    let node = cache
        .get(&target.object)
        .ok_or(SemanticError::StaleIdentity)?;
    if node.application_generation != target.application_generation
        || node.revision != target.node_revision
        || node.item.application != target.application
        || node.identity_fingerprint != target.identity_fingerprint
        || node.item.index_in_parent != target.index_in_parent
        || node.item.role != target.role
        || node.item.states != target.states
    {
        return Err(SemanticError::StaleIdentity);
    }
    for interface in request.operation.required_interfaces() {
        if !node
            .item
            .interfaces
            .iter()
            .any(|candidate| candidate == interface)
        {
            return Err(SemanticError::InterfaceUnavailable(interface));
        }
    }
    if request.operation.is_text_write() && node.item.text_protection == TextProtection::Unknown {
        return Err(SemanticError::UnclassifiedTextDenied);
    }
    if node.item.text_protection == TextProtection::Protected
        && request.operation.text_verification()
            == Some(crate::semantic::TextVerificationMode::Exact)
    {
        return Err(SemanticError::InvalidRequest(
            "exact verification is denied for protected text",
        ));
    }
    Ok(())
}

async fn run_disabled(
    requests: &mut mpsc::Receiver<ActorRequest>,
    health: &watch::Sender<AtspiActorHealth>,
    shutdown: &CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            request = requests.recv() => match request {
                Some(request) => respond_transitional(request, health),
                None => break,
            }
        }
    }
}

fn respond_transitional(request: ActorRequest, health: &watch::Sender<AtspiActorHealth>) {
    match request {
        ActorRequest::Snapshot {
            cancellation,
            reply,
        } => {
            let result = if cancellation.is_cancelled() {
                Err(AtspiActorError::Cancelled)
            } else {
                Ok(AtspiActorSnapshot {
                    health: health.borrow().clone(),
                })
            };
            let _ignored = reply.send(result);
        }
        ActorRequest::Rebuild {
            cancellation,
            reply,
        } => {
            let result = if cancellation.is_cancelled() {
                Err(AtspiActorError::Cancelled)
            } else {
                Err(AtspiActorError::Unavailable(health.borrow().state))
            };
            let _ignored = reply.send(result);
        }
        ActorRequest::CachePage {
            cancellation,
            reply,
            ..
        } => {
            let result = if cancellation.is_cancelled() {
                Err(AtspiActorError::Cancelled)
            } else {
                Err(AtspiActorError::Unavailable(health.borrow().state))
            };
            let _ignored = reply.send(result);
        }
        ActorRequest::Semantic {
            request,
            cancellation,
            reply,
        } => {
            let result = if cancellation.is_cancelled() {
                Err(SemanticError::CancelledBeforeDispatch)
            } else if tokio::time::Instant::now() >= request.deadline {
                Err(SemanticError::DeadlineBeforeDispatch)
            } else {
                Err(SemanticError::Unavailable)
            };
            let _ignored = reply.send(result);
        }
        ActorRequest::SemanticTarget {
            cancellation,
            reply,
            ..
        } => {
            let result = transitional_semantic_target_result(&cancellation);
            let _ignored = reply.send(result);
        }
        ActorRequest::SemanticTargetBlocking {
            cancellation,
            reply,
            ..
        } => {
            let result = transitional_semantic_target_result(&cancellation);
            let _ignored = reply.try_send(result);
        }
        ActorRequest::ReconcileSemanticTarget {
            deadline,
            cancellation,
            reply,
            ..
        } => {
            let result = if cancellation.is_cancelled() {
                Err(SemanticError::CancelledBeforeDispatch)
            } else if tokio::time::Instant::now() >= deadline {
                Err(SemanticError::DeadlineBeforeDispatch)
            } else {
                Err(SemanticError::Unavailable)
            };
            let _ignored = reply.send(result);
        }
        ActorRequest::Observe {
            request,
            cancellation,
            reply,
        } => {
            let result = transitional_observation_result(&request, &cancellation);
            let _ignored = reply.send(result);
        }
        ActorRequest::ObserveBlocking {
            request,
            cancellation,
            reply,
        } => {
            let result = transitional_observation_result(&request, &cancellation);
            let _ignored = reply.try_send(result);
        }
    }
}

fn transitional_semantic_target_result(
    cancellation: &CancellationToken,
) -> Result<SemanticTarget, SemanticError> {
    if cancellation.is_cancelled() {
        Err(SemanticError::CancelledBeforeDispatch)
    } else {
        Err(SemanticError::Unavailable)
    }
}

fn transitional_observation_result(
    request: &SemanticObservationRequest,
    cancellation: &CancellationToken,
) -> Result<SemanticObservationResult, SemanticError> {
    if cancellation.is_cancelled() {
        Err(SemanticError::CancelledBeforeDispatch)
    } else if tokio::time::Instant::now() >= request.deadline {
        Err(SemanticError::DeadlineBeforeDispatch)
    } else {
        Err(SemanticError::Unavailable)
    }
}

async fn wait_reconnect_delay(
    delay: Duration,
    requests: &mut mpsc::Receiver<ActorRequest>,
    health: &watch::Sender<AtspiActorHealth>,
    shutdown: &CancellationToken,
) -> bool {
    let sleeper = sleep(delay);
    tokio::pin!(sleeper);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return true,
            () = &mut sleeper => return false,
            request = requests.recv() => match request {
                Some(request) => respond_transitional(request, health),
                None => return true,
            }
        }
    }
}

fn next_delay(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

fn stable_empty_epoch(candidate: u64, confirmed: u64) -> bool {
    candidate == confirmed && confirmed & 1 == 0
}

fn refresh_cache_health(sender: &watch::Sender<AtspiActorHealth>, cache: &BoundedCache) {
    let mut health = sender.borrow().clone();
    health.cache_revision = cache.revision();
    health.cached_nodes = cache.len();
    health.cached_bytes = cache.bytes();
    sender.send_replace(health);
}

fn update_health(
    sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    state: AtspiActorState,
    cache: &BoundedCache,
    error: Option<String>,
) {
    let mut health = sender.borrow().clone();
    health.state = state;
    health.cache_revision = cache.revision();
    health.cached_nodes = cache.len();
    health.cached_bytes = cache.bytes();
    health.last_error = error;
    publish_health(sender, emitter, health);
}

fn invalidate_generation(
    sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    cache: &mut BoundedCache,
    reason: &'static str,
    error: Option<String>,
) -> bool {
    let mut health = sender.borrow().clone();
    let Some(accessibility_generation) = health.accessibility_generation.checked_add(1) else {
        health.last_error = Some("accessibility generation exhausted".to_owned());
        publish_health(sender, emitter, health);
        return false;
    };
    if let Err(error) = cache.invalidate_all() {
        health.last_error = Some(error.to_string());
        publish_health(sender, emitter, health);
        return false;
    }
    health.state = AtspiActorState::Reconnecting;
    health.accessibility_generation = accessibility_generation;
    health.cache_revision = cache.revision();
    health.cached_nodes = 0;
    health.cached_bytes = 0;
    health.last_error = error;
    publish_health(sender, emitter, health.clone());
    emitter.offer(AtspiActorEvent::ResyncRequired {
        accessibility_generation: health.accessibility_generation,
        cache_revision: health.cache_revision,
        reason,
    });
    true
}

fn stop_health(
    sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    cache: &BoundedCache,
) {
    let last_error = sender.borrow().last_error.clone();
    update_health(sender, emitter, AtspiActorState::Stopped, cache, last_error);
}

fn publish_health(
    sender: &watch::Sender<AtspiActorHealth>,
    emitter: &ActorEventEmitter,
    health: AtspiActorHealth,
) {
    sender.send_replace(health.clone());
    emitter.offer(AtspiActorEvent::HealthChanged(health));
}

/// Error returned by typed actor requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AtspiActorError {
    /// Request queue reached its configured capacity.
    #[error("AT-SPI actor request queue is full")]
    QueueFull,
    /// Caller cancelled before a result was returned.
    #[error("AT-SPI actor request was cancelled")]
    Cancelled,
    /// Actor is not currently healthy enough for this operation.
    #[error("AT-SPI actor is unavailable in state {0:?}")]
    Unavailable(AtspiActorState),
    /// A continuation revision no longer matches current actor cache state.
    #[error("AT-SPI cache revision changed: expected {expected}, current {current}")]
    StaleRevision {
        /// Revision supplied by the caller.
        expected: u64,
        /// Current actor-owned cache revision.
        current: u64,
    },
    /// Accessibility generation changed across a paginated snapshot.
    #[error("AT-SPI accessibility generation changed: expected {expected}, current {current}")]
    StaleGeneration {
        /// Generation supplied by the caller.
        expected: u64,
        /// Current actor-owned generation.
        current: u64,
    },
    /// Cache page policy could not admit even one deterministic result.
    #[error("AT-SPI cache page exceeds configured read limits")]
    InvalidPage,
    /// Actor has stopped or its request channel closed.
    #[error("AT-SPI actor stopped")]
    Stopped,
}

/// Configuration or startup error.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AtspiActorSpawnError {
    /// Queue/deadline/reconnect configuration is invalid.
    #[error("invalid AT-SPI actor configuration: {0}")]
    InvalidConfig(&'static str),
    /// Cache limits are invalid.
    #[error(transparent)]
    Cache(CacheError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::mpsc as std_mpsc,
        time::Duration,
    };

    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::{
        ActorEventEmitter, ActorRequest, AtspiActorConfig, AtspiActorHealth, AtspiBackend,
        AtspiHandle, BackendFailure, BackendFailureKind, BackendFuture, BackendRefreshRequest,
        BoundedCache, CacheLimits, ObjectAddress, RefreshedCacheItem, SemanticError,
        SemanticTarget, SemanticTargetRequest, reconcile_dirty_objects, respond_transitional,
        stable_empty_epoch,
    };
    use crate::semantic::IdentityFingerprint;

    #[derive(Debug, Default)]
    struct RefreshCountingBackend {
        refresh_calls: usize,
    }

    impl AtspiBackend for RefreshCountingBackend {
        fn bootstrap(
            &mut self,
            _limits: CacheLimits,
            _proxy_call_timeout: std::time::Duration,
        ) -> BackendFuture<'_, Result<Vec<crate::cache::NormalizedCacheItem>, BackendFailure>>
        {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn refresh_object(
            &mut self,
            _request: BackendRefreshRequest,
        ) -> BackendFuture<'_, Result<RefreshedCacheItem, BackendFailure>> {
            self.refresh_calls += 1;
            Box::pin(async {
                Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "unexpected test refresh",
                ))
            })
        }

        fn shutdown(&mut self) -> BackendFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn empty_queue_admission_requires_an_unchanged_even_ingress_epoch() {
        assert!(stable_empty_epoch(8, 8));
        assert!(!stable_empty_epoch(8, 10));
        assert!(!stable_empty_epoch(9, 9));
        assert!(!stable_empty_epoch(9, 10));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_semantic_target_delivers_a_synchronous_success_reply()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = semantic_target_request()?;
        let expected = semantic_target(&request);
        let reply = expected.clone();
        let (request_sender, mut requests) = mpsc::channel(1);
        let (_health_sender, health) = watch::channel(AtspiActorHealth::initial(true));
        let shutdown = CancellationToken::new();
        let handle = AtspiHandle {
            requests: request_sender,
            health,
            shutdown,
        };
        let responder = tokio::spawn(async move {
            let admitted = requests
                .recv()
                .await
                .ok_or("blocking semantic-target request channel closed")?;
            let ActorRequest::SemanticTargetBlocking { reply: sender, .. } = admitted else {
                return Err("blocking semantic-target request was not admitted");
            };
            sender
                .try_send(Ok(reply))
                .map_err(|_| "blocking semantic-target reply receiver closed")
        });

        let result = tokio::task::spawn_blocking(move || {
            handle.semantic_target_blocking(request, Duration::from_secs(1))
        })
        .await??;

        responder.await?.map_err(std::io::Error::other)?;
        assert_eq!(result, expected);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_semantic_target_timeout_cancels_the_admitted_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let (request_sender, mut requests) = mpsc::channel(1);
        let (_health_sender, health) = watch::channel(AtspiActorHealth::initial(true));
        let shutdown = CancellationToken::new();
        let handle = AtspiHandle {
            requests: request_sender,
            health,
            shutdown,
        };
        let request = semantic_target_request()?;

        let result = tokio::task::spawn_blocking(move || {
            handle.semantic_target_blocking(request, Duration::from_millis(20))
        })
        .await?;

        assert_eq!(result, Err(SemanticError::DeadlineBeforeDispatch));
        let admitted = requests
            .recv()
            .await
            .ok_or("timed-out semantic-target request channel closed")?;
        let ActorRequest::SemanticTargetBlocking { cancellation, .. } = admitted else {
            return Err("timed-out blocking semantic-target request was not admitted".into());
        };
        assert!(cancellation.is_cancelled());
        Ok(())
    }

    #[test]
    fn blocking_semantic_target_fails_closed_in_transitional_states()
    -> Result<(), Box<dyn std::error::Error>> {
        let (health, _receiver) = watch::channel(AtspiActorHealth::initial(true));

        for (cancelled, expected) in [
            (false, SemanticError::Unavailable),
            (true, SemanticError::CancelledBeforeDispatch),
        ] {
            let cancellation = CancellationToken::new();
            if cancelled {
                cancellation.cancel();
            }
            let (reply, receiver) = std_mpsc::sync_channel(1);
            respond_transitional(
                ActorRequest::SemanticTargetBlocking {
                    request: semantic_target_request()?,
                    cancellation,
                    reply,
                },
                &health,
            );
            assert_eq!(
                receiver
                    .recv_timeout(Duration::from_millis(20))
                    .map_err(std::io::Error::other)?,
                Err(expected)
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn dirty_object_absent_from_authoritative_cache_is_suppressed()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = AtspiActorConfig::default();
        let mut cache = BoundedCache::new(config.cache_limits)?;
        let missing_object = ObjectAddress::new(":1.404", "/test/transient")?;
        let mut pending_refreshes = BTreeMap::from([(
            missing_object,
            BTreeSet::from(["object:state-changed:focused".to_owned()]),
        )]);
        let mut backend = RefreshCountingBackend::default();
        let (health_sender, _health_receiver) =
            tokio::sync::watch::channel(AtspiActorHealth::initial(true));
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::channel(1);
        let (overflow_sender, overflow_receiver) = tokio::sync::watch::channel(0_u64);
        let emitter = ActorEventEmitter {
            sender: event_sender,
            overflow: overflow_sender,
        };

        let reconnect_reason = reconcile_dirty_objects(
            &mut backend,
            &mut cache,
            &health_sender,
            &emitter,
            config,
            &mut pending_refreshes,
        )
        .await;

        assert_eq!(reconnect_reason, None);
        assert_eq!(backend.refresh_calls, 0);
        assert!(pending_refreshes.is_empty());
        assert!(matches!(
            event_receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(*overflow_receiver.borrow(), 0);
        Ok(())
    }

    fn semantic_target_request() -> Result<SemanticTargetRequest, crate::cache::CacheError> {
        Ok(SemanticTargetRequest {
            object: ObjectAddress::new(":1.500", "/test/object")?,
            application: ObjectAddress::new(":1.500", "/test/application")?,
            accessibility_generation: 7,
            application_generation: 11,
            cache_revision: 13,
            node_revision: 17,
        })
    }

    fn semantic_target(request: &SemanticTargetRequest) -> SemanticTarget {
        SemanticTarget {
            object: request.object.clone(),
            application: request.application.clone(),
            accessibility_generation: request.accessibility_generation,
            application_generation: request.application_generation,
            cache_revision: request.cache_revision,
            node_revision: request.node_revision,
            index_in_parent: Some(2),
            identity_fingerprint: IdentityFingerprint::from_parts(
                &request.object,
                &request.application,
                None,
                Some(2),
                "test object",
                "blocking semantic-target seam",
            ),
            role: 42,
            states: vec![1, 2],
        }
    }
}
