//! Central-bus AT-SPI backend for the single-owner actor.
//!
//! The signal stream is installed before Registry forwarding and Cache
//! `GetItems`, so incremental updates are queued while the snapshot is read.
//! This module deliberately does not call `AccessibilityConnection::register_event`,
//! `deregister_event`, or `remove_match_rule`: the pinned connection crate's
//! removal helper adds the match rule again. `MessageStream` owns the D-Bus
//! match lifetime and only the correct Registry forwarding helpers are used.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    sync::Arc,
    time::Duration,
};

use atspi_connection::{
    AccessibilityConnection,
    common::events::{CacheEvents, FocusEvents, ObjectEvents, WindowEvents},
};
use atspi_proxies::{
    action::ActionProxy,
    common::{CoordType, ScrollType},
    component::ComponentProxy,
    editable_text::EditableTextProxy,
    selection::SelectionProxy,
    text::TextProxy,
    value::ValueProxy,
};
use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use zbus::{
    MatchRule, MessageStream,
    message::{Sequence, Type as MessageType},
    proxy::CacheProperties,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use crate::cache::application_root_has_external_parent;
use crate::semantic::{
    ActionEvidence, ActionSelector, BackendObservationRequest, BackendSemanticRequest,
    IdentityFingerprint, MAX_ACTION_EVIDENCE_BYTES, MAX_ACTION_FIELD_BYTES, MAX_ACTIONS,
    MAX_SELECTION_RANGES, ScrollPlacement, SelectionOperation, SelectionRangeEvidence,
    SemanticDispatchMarker, SemanticEvidence, SemanticObservationEvidence, SemanticOperation,
    SemanticRect, SemanticValueEvidence, TextInsertPosition, TextReadbackEvidence,
    TextSelectionPolicy, TextVerificationMode,
};
use crate::{
    AtspiBackend, AtspiBackendConnector, BackendEvent, BackendEventIngress, BackendFailure,
    BackendFailureKind, BackendFuture, BackendRefreshRequest, CacheEvent, CacheLimits,
    CachedLiveMetadata, CachedTextMetadata, EventOfferResult, LegacyCacheItem, ModernCacheItem,
    NormalizedCacheItem, ObjectAddress, RefreshedCacheItem, normalize_legacy, normalize_modern,
};

const MODERN_CACHE_SIGNATURE: &str = "a((so)(so)(so)iiassusau)";
const LEGACY_CACHE_SIGNATURE: &str = "a((so)(so)(so)a(so)assusau)";
const MODERN_CACHE_ITEM_SIGNATURE: &str = "((so)(so)(so)iiassusau)";
const LEGACY_CACHE_ITEM_SIGNATURE: &str = "((so)(so)(so)a(so)assusau)";
const REMOVE_CACHE_ITEM_SIGNATURE: &str = "(so)";
const MAX_LAZY_TRAVERSAL_DEPTH: usize = 64;
const DEGRADED_RETRY_DELAY: Duration = Duration::from_secs(5);
const SEMANTIC_SETTLE_INITIAL_DELAY: Duration = Duration::from_millis(10);
const SEMANTIC_SETTLE_MAX_DELAY: Duration = Duration::from_millis(100);
const SEMANTIC_SETTLE_MAX_ATTEMPTS: usize = 32;
// Pinned zbus 5.18 rejects a complete wire message above 128 MiB. This is a
// transport allocation bound, not the much tighter decoded cache-item limit.
const MAX_RAW_ATSPI_MESSAGE_BYTES: usize = 128 * 1_024 * 1_024;
// A D-Bus string body is its u32 byte length, UTF-8 bytes, and trailing NUL.
// Bound the raw body before zvariant exposes even a borrowed string. The shared
// zbus connection still has its upstream 128 MiB raw-message allocation bound;
// zbus 5.18 does not expose a smaller per-call receive limit.
const DBUS_STRING_BODY_OVERHEAD_BYTES: usize =
    std::mem::size_of::<u32>() + std::mem::size_of::<u8>();
const MAX_EXACT_TEXT_REPLY_BODY_BYTES: usize =
    crate::semantic::MAX_SEMANTIC_TEXT_BYTES + DBUS_STRING_BODY_OVERHEAD_BYTES;
const _: () = assert!(DBUS_STRING_BODY_OVERHEAD_BYTES == 5);

#[derive(Clone, Debug)]
struct SemanticSettle {
    deadline: Instant,
    delay: Duration,
    attempts_remaining: usize,
}

impl SemanticSettle {
    fn new(terminal_deadline: Instant, settle_budget: Duration) -> Self {
        Self {
            deadline: terminal_deadline.min(Instant::now() + settle_budget),
            delay: SEMANTIC_SETTLE_INITIAL_DELAY,
            attempts_remaining: SEMANTIC_SETTLE_MAX_ATTEMPTS,
        }
    }

    fn next_call_timeout(&mut self, call_ceiling: Duration) -> Option<Duration> {
        if self.attempts_remaining == 0 {
            return None;
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        self.attempts_remaining -= 1;
        Some(remaining.min(call_ceiling))
    }

    fn next_pause_duration(&mut self) -> Option<Duration> {
        if self.attempts_remaining == 0 {
            return None;
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let delay = self.delay.min(remaining);
        self.delay = self.delay.saturating_mul(2).min(SEMANTIC_SETTLE_MAX_DELAY);
        Some(delay)
    }

    async fn pause(&mut self) -> bool {
        let Some(delay) = self.next_pause_duration() else {
            return false;
        };
        sleep(delay).await;
        Instant::now() < self.deadline
    }
}

fn finish_semantic_settle<T>(
    last_valid: Option<T>,
    last_timeout: Option<BackendFailure>,
    empty_message: &'static str,
) -> Result<T, BackendFailure> {
    last_valid.ok_or_else(|| {
        last_timeout
            .unwrap_or_else(|| BackendFailure::new(BackendFailureKind::Timeout, empty_message))
    })
}
const SIGNAL_STREAM_COUNT: usize = 5;

fn partition_raw_signal_capacity(aggregate: usize) -> Option<[usize; SIGNAL_STREAM_COUNT]> {
    if !(SIGNAL_STREAM_COUNT..=LiveAtspiConnector::MAX_RAW_SIGNAL_QUEUE_CAPACITY)
        .contains(&aggregate)
    {
        return None;
    }
    let base = aggregate / SIGNAL_STREAM_COUNT;
    let remainder = aggregate % SIGNAL_STREAM_COUNT;
    Some(std::array::from_fn(|index| {
        base + usize::from(index < remainder)
    }))
}

type RawObjectRef = (String, OwnedObjectPath);
type RawModernCacheItem = (
    RawObjectRef,
    RawObjectRef,
    RawObjectRef,
    i32,
    i32,
    Vec<String>,
    String,
    u32,
    String,
    Vec<u32>,
);
type RawLegacyCacheItem = (
    RawObjectRef,
    RawObjectRef,
    RawObjectRef,
    Vec<RawObjectRef>,
    Vec<String>,
    String,
    u32,
    String,
    Vec<u32>,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationBootstrapBarrier {
    Ready(Sequence),
    LazyReady(Sequence),
    Degraded,
}

type BootstrapBarriers = BTreeMap<String, ApplicationBootstrapBarrier>;

#[derive(Debug)]
struct SequencedCacheEvent {
    event: BackendEvent,
    owner: String,
    position: Sequence,
}

#[derive(Debug, Eq, PartialEq)]
struct ObservedObjectSignal {
    source: ObjectAddress,
    kind: &'static str,
    policy: ObjectEventPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectEventPolicy {
    Forward,
    Refresh,
    Resync,
}

#[derive(Debug)]
struct SignalDrainContext {
    ingress: BackendEventIngress,
    cache_limits: CacheLimits,
    buffer_capacity: usize,
    bootstrap_barriers: watch::Receiver<Option<Arc<BootstrapBarriers>>>,
    bootstrap_barriers_applied: watch::Sender<bool>,
    cancellation: CancellationToken,
    connection: zbus::Connection,
}

#[derive(Debug)]
struct LiveSemanticIdentity {
    object: ObjectAddress,
    application: ObjectAddress,
    expected_identity: IdentityFingerprint,
    expected_index_in_parent: Option<usize>,
    expected_role: u32,
    expected_states: Vec<u32>,
    proxy_call_timeout: Duration,
    cache_limits: CacheLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LazyAccessibleNode {
    application: ObjectAddress,
    parent: Option<ObjectAddress>,
    index_in_parent: i32,
    child_count: i32,
    children: Vec<ObjectAddress>,
    interfaces: Vec<String>,
    name: String,
    description: String,
    role: u32,
    states: Vec<u32>,
}

trait LazyAccessibleSource {
    fn read_node<'a>(
        &'a mut self,
        object: &'a ObjectAddress,
        call_timeout: Duration,
        limits: CacheLimits,
    ) -> BackendFuture<'a, Result<LazyAccessibleNode, BackendFailure>>;
}

#[derive(Debug)]
struct LiveLazyAccessibleSource<'a> {
    connection: &'a zbus::Connection,
}

/// Production central-bus connector with an explicit zbus signal queue bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveAtspiConnector {
    raw_signal_queue_capacity: usize,
    decoded_event_capacity: usize,
}

impl LiveAtspiConnector {
    /// Smallest aggregate capacity that gives each filtered raw stream one slot.
    pub const MIN_RAW_SIGNAL_QUEUE_CAPACITY: usize = SIGNAL_STREAM_COUNT;
    /// Hard aggregate raw-message cardinality ceiling for explicit callers.
    pub const MAX_RAW_SIGNAL_QUEUE_CAPACITY: usize = 512;

    /// Construct a connector with separate transport and decoded-event bounds.
    ///
    /// `raw_signal_queue_capacity` is partitioned exactly across the five zbus
    /// streams. Under pinned zbus, each raw slot can retain a complete wire
    /// message of up to 128 MiB before decoding. `decoded_event_capacity` bounds
    /// the bootstrap cache-event buffer; each admitted cache event is independently
    /// limited by `CacheLimits::max_item_bytes`. Invalid values are rejected by
    /// `connect`.
    #[must_use]
    pub const fn new(raw_signal_queue_capacity: usize, decoded_event_capacity: usize) -> Self {
        Self {
            raw_signal_queue_capacity,
            decoded_event_capacity,
        }
    }

    /// Configured aggregate raw zbus signal cardinality across all five streams.
    #[must_use]
    pub const fn raw_signal_queue_capacity(self) -> usize {
        self.raw_signal_queue_capacity
    }

    /// Configured decoded bootstrap-event buffer cardinality.
    #[must_use]
    pub const fn decoded_event_capacity(self) -> usize {
        self.decoded_event_capacity
    }

    /// Worst-case bytes retained by the raw queues under pinned zbus framing.
    #[must_use]
    pub const fn raw_signal_queue_worst_case_bytes(self) -> Option<usize> {
        self.raw_signal_queue_capacity
            .checked_mul(MAX_RAW_ATSPI_MESSAGE_BYTES)
    }
}

impl Default for LiveAtspiConnector {
    fn default() -> Self {
        Self::new(Self::MIN_RAW_SIGNAL_QUEUE_CAPACITY, 128)
    }
}

async fn bounded_signal_stream(
    connection: &zbus::Connection,
    interface: &'static str,
    capacity: usize,
) -> Result<MessageStream, BackendFailure> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(interface)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
        .build();
    MessageStream::for_match_rule(rule, connection, Some(capacity))
        .await
        .map_err(|error| backend_error(BackendFailureKind::Connection, error))
}

impl AtspiBackendConnector for LiveAtspiConnector {
    type Backend = LiveAtspiBackend;

    fn connect(
        &mut self,
        ingress: BackendEventIngress,
        cache_limits: CacheLimits,
    ) -> BackendFuture<'_, Result<Self::Backend, BackendFailure>> {
        let raw_signal_queue_capacity = self.raw_signal_queue_capacity;
        let decoded_event_capacity = self.decoded_event_capacity;
        Box::pin(async move {
            let raw_stream_capacities = partition_raw_signal_capacity(raw_signal_queue_capacity)
                .ok_or_else(|| {
                    BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "aggregate raw signal capacity is outside the supported bounds",
                    )
                })?;
            if decoded_event_capacity == 0 {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "decoded event buffer capacity must be nonzero",
                ));
            }
            let byte_weighted_decoded_ceiling = cache_limits
                .max_total_bytes
                .checked_div(cache_limits.max_item_bytes)
                .unwrap_or(0)
                .max(1);
            if decoded_event_capacity > byte_weighted_decoded_ceiling {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "decoded event buffer exceeds its byte-derived capacity",
                ));
            }
            if raw_signal_queue_capacity
                .checked_mul(MAX_RAW_ATSPI_MESSAGE_BYTES)
                .is_none()
            {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "aggregate raw signal cardinality exceeds the configured safety ceiling",
                ));
            }
            let connection = AccessibilityConnection::new()
                .await
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            let rule = MatchRule::builder()
                .msg_type(MessageType::Signal)
                .interface("org.a11y.atspi.Cache")
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .build();
            let stream = MessageStream::for_match_rule(
                rule,
                connection.connection(),
                Some(raw_stream_capacities[0]),
            )
            .await
            .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            let owner_rule = MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender("org.freedesktop.DBus")
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .path("/org/freedesktop/DBus")
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .interface("org.freedesktop.DBus")
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .member("NameOwnerChanged")
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .build();
            let owner_stream = MessageStream::for_match_rule(
                owner_rule,
                connection.connection(),
                Some(raw_stream_capacities[1]),
            )
            .await
            .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            let object_stream = bounded_signal_stream(
                connection.connection(),
                "org.a11y.atspi.Event.Object",
                raw_stream_capacities[2],
            )
            .await?;
            let focus_stream = bounded_signal_stream(
                connection.connection(),
                "org.a11y.atspi.Event.Focus",
                raw_stream_capacities[3],
            )
            .await?;
            let window_stream = bounded_signal_stream(
                connection.connection(),
                "org.a11y.atspi.Event.Window",
                raw_stream_capacities[4],
            )
            .await?;

            // Merge the independently filtered queues by zbus's global receive
            // sequence before applying the bootstrap and owner-change fences.
            let ordered_streams = vec![
                ordered_stream::OrderedStreamExt::peekable(stream),
                ordered_stream::OrderedStreamExt::peekable(owner_stream),
                ordered_stream::OrderedStreamExt::peekable(object_stream),
                ordered_stream::OrderedStreamExt::peekable(focus_stream),
                ordered_stream::OrderedStreamExt::peekable(window_stream),
            ];
            let stream = ordered_stream::OrderedStreamExt::into_stream(
                ordered_stream::JoinMultiple(ordered_streams),
            );

            // MessageStream installed its correct AddMatch/Drop removal first.
            // Registry forwarding is then enabled without the broken wrapper.
            connection
                .add_registry_event::<CacheEvents>()
                .await
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            connection
                .add_registry_event::<ObjectEvents>()
                .await
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            connection
                .add_registry_event::<FocusEvents>()
                .await
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            connection
                .add_registry_event::<WindowEvents>()
                .await
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;

            let drain_cancellation = CancellationToken::new();
            let (bootstrap_barriers, barrier_receiver) = watch::channel(None);
            let (barriers_applied_sender, bootstrap_barriers_applied) = watch::channel(false);
            let drain = tokio::spawn(drain_cache_signals(
                stream,
                SignalDrainContext {
                    ingress: ingress.clone(),
                    cache_limits,
                    buffer_capacity: decoded_event_capacity,
                    bootstrap_barriers: barrier_receiver,
                    bootstrap_barriers_applied: barriers_applied_sender,
                    cancellation: drain_cancellation.clone(),
                    connection: connection.connection().clone(),
                },
            ));
            Ok(LiveAtspiBackend {
                connection,
                ingress,
                bootstrap_barriers,
                bootstrap_barriers_applied,
                drain_cancellation,
                drain: Some(drain),
                degraded_retry: None,
            })
        })
    }
}

/// One actor-owned live connection generation. It is never exposed by the handle.
#[derive(Debug)]
pub struct LiveAtspiBackend {
    connection: AccessibilityConnection,
    ingress: BackendEventIngress,
    bootstrap_barriers: watch::Sender<Option<Arc<BootstrapBarriers>>>,
    bootstrap_barriers_applied: watch::Receiver<bool>,
    drain_cancellation: CancellationToken,
    drain: Option<JoinHandle<()>>,
    degraded_retry: Option<JoinHandle<()>>,
}

impl LazyAccessibleSource for LiveLazyAccessibleSource<'_> {
    fn read_node<'a>(
        &'a mut self,
        object: &'a ObjectAddress,
        call_timeout: Duration,
        limits: CacheLimits,
    ) -> BackendFuture<'a, Result<LazyAccessibleNode, BackendFailure>> {
        Box::pin(read_live_lazy_node(
            self.connection,
            object,
            call_timeout,
            limits,
            true,
        ))
    }
}

fn bounded_child_count(
    child_count: i32,
    enumerate_children: bool,
    limits: CacheLimits,
) -> Result<usize, BackendFailure> {
    let count = match usize::try_from(child_count) {
        Ok(count) => count,
        // AT-SPI permits -1 when a virtualized accessible cannot report a
        // stable child count. Shallow exact refresh does not enumerate the
        // children, so it can still hydrate the requested live interfaces.
        Err(_) if !enumerate_children && child_count == -1 => 0,
        Err(_) => {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Accessible.ChildCount was negative",
            ));
        }
    };
    if count > limits.max_children || count > limits.max_nodes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Accessible.ChildCount exceeded the bounded traversal limit",
        ));
    }
    Ok(count)
}

async fn read_live_lazy_node(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    limits: CacheLimits,
    enumerate_children: bool,
) -> Result<LazyAccessibleNode, BackendFailure> {
    let application = raw_required_object_ref(
        &bounded_accessible_empty_reply(
            connection,
            object,
            "GetApplication",
            call_timeout,
            limits.max_item_bytes,
        )
        .await?,
    )?;
    let parent = raw_optional_object_property(
        connection,
        object,
        "Parent",
        call_timeout,
        limits.max_item_bytes,
    )
    .await?;
    let index_in_parent = bounded_accessible_empty_reply(
        connection,
        object,
        "GetIndexInParent",
        call_timeout,
        limits.max_item_bytes,
    )
    .await?
    .body()
    .deserialize::<i32>()
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let child_count = raw_i32_property(
        connection,
        object,
        "ChildCount",
        call_timeout,
        limits.max_item_bytes,
    )
    .await?;
    let child_count_usize = bounded_child_count(child_count, enumerate_children, limits)?;
    let mut children = Vec::new();
    if enumerate_children {
        let address_floor = std::mem::size_of::<ObjectAddress>().max(1);
        let initial_capacity = child_count_usize.min(limits.max_item_bytes / address_floor);
        children.reserve(initial_capacity);
        let mut child_address_bytes = 0_usize;
        for index in 0..child_count_usize {
            let index = i32::try_from(index).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "child index overflow")
            })?;
            let reply = bounded_accessible_index_reply(
                connection,
                object,
                "GetChildAtIndex",
                index,
                call_timeout,
                limits.max_item_bytes,
            )
            .await?;
            let child = raw_required_object_ref(&reply)?;
            child_address_bytes = child_address_bytes
                .checked_add(child.bus_name().len() + child.object_path().len())
                .ok_or_else(|| {
                    BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "child address byte accounting overflowed",
                    )
                })?;
            if child_address_bytes > limits.max_item_bytes {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "child addresses exceeded the aggregate node byte limit",
                ));
            }
            children.push(child);
        }
    }
    let interfaces = read_raw_interfaces(
        connection,
        object,
        call_timeout,
        limits.max_item_bytes,
        limits.max_interfaces,
    )
    .await?;
    let name = raw_string_property(
        connection,
        object,
        "Name",
        call_timeout,
        limits.max_item_bytes,
        limits.max_string_bytes,
    )
    .await?;
    let description = raw_string_property(
        connection,
        object,
        "Description",
        call_timeout,
        limits.max_item_bytes,
        limits.max_string_bytes,
    )
    .await?;
    let role = read_raw_role(connection, object, call_timeout, limits.max_item_bytes).await?;
    let states = read_raw_states(
        connection,
        object,
        call_timeout,
        limits.max_item_bytes,
        limits.max_states,
    )
    .await?;
    let node = LazyAccessibleNode {
        application,
        parent,
        index_in_parent,
        child_count,
        children,
        interfaces,
        name,
        description,
        role,
        states,
    };
    validate_lazy_node_bytes(&node, limits)?;
    Ok(node)
}

#[derive(Debug, Eq, PartialEq)]
struct LazyTraversalPass {
    items: Vec<NormalizedCacheItem>,
    fingerprint: [u8; 32],
}

async fn traverse_lazy_application<S: LazyAccessibleSource>(
    source: &mut S,
    application_root: ObjectAddress,
    limits: CacheLimits,
    call_timeout: Duration,
) -> Result<Vec<NormalizedCacheItem>, BackendFailure> {
    limits
        .validate()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let expected_fingerprint = {
        let first =
            collect_lazy_traversal_pass(source, &application_root, limits, call_timeout).await?;
        first.fingerprint
    };
    let verified =
        collect_lazy_traversal_pass(source, &application_root, limits, call_timeout).await?;
    if expected_fingerprint != verified.fingerprint {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility tree changed while it was being verified",
        ));
    }
    Ok(verified.items)
}

async fn collect_lazy_traversal_pass<S: LazyAccessibleSource>(
    source: &mut S,
    application_root: &ObjectAddress,
    limits: CacheLimits,
    call_timeout: Duration,
) -> Result<LazyTraversalPass, BackendFailure> {
    let depth_limit = limits.max_nodes.min(MAX_LAZY_TRAVERSAL_DEPTH);
    let mut work = VecDeque::from([(application_root.clone(), None, None, 0_usize)]);
    let mut discovered = BTreeSet::from([application_root.clone()]);
    let mut items = Vec::new();
    let mut aggregate_bytes = 0_usize;
    let mut fingerprint = Sha256::new();

    while let Some((object, expected_parent, expected_index, depth)) = work.pop_front() {
        if depth > depth_limit {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "lazy accessibility traversal depth limit exceeded",
            ));
        }
        let node = timeout(
            call_timeout,
            source.read_node(&object, call_timeout, limits),
        )
        .await
        .map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Timeout,
                "lazy accessibility node read timed out",
            )
        })??;
        validate_lazy_node(
            &object,
            application_root,
            expected_parent.as_ref(),
            expected_index,
            &node,
            limits,
        )?;
        for (index, child) in node.children.iter().enumerate() {
            if !discovered.insert(child.clone()) {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "lazy accessibility tree contained a cycle or duplicate child",
                ));
            }
            if discovered.len() > limits.max_nodes {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "lazy accessibility traversal node limit exceeded",
                ));
            }
            work.push_back((
                child.clone(),
                Some(object.clone()),
                Some(index),
                depth.saturating_add(1),
            ));
        }
        let item = normalize_modern(
            ModernCacheItem {
                object,
                application: node.application,
                parent: node.parent,
                index_in_parent: node.index_in_parent,
                child_count: node.child_count,
                interfaces: node.interfaces,
                short_name: node.name,
                role: node.role,
                name: node.description,
                states: node.states,
            },
            limits,
        )
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
        aggregate_bytes =
            account_bootstrap_items(aggregate_bytes, std::slice::from_ref(&item), limits)?;
        hash_lazy_item(&mut fingerprint, &item, &node.children);
        items.push(item);
    }

    Ok(LazyTraversalPass {
        items,
        fingerprint: fingerprint.finalize().into(),
    })
}

fn hash_lazy_item(
    fingerprint: &mut Sha256,
    item: &NormalizedCacheItem,
    children: &[ObjectAddress],
) {
    hash_address(fingerprint, &item.object);
    hash_address(fingerprint, &item.application);
    match &item.parent {
        Some(parent) => {
            fingerprint.update([1]);
            hash_address(fingerprint, parent);
        }
        None => fingerprint.update([0]),
    }
    hash_usize(fingerprint, item.index_in_parent);
    hash_usize(fingerprint, item.child_count);
    hash_strings(fingerprint, &item.interfaces);
    hash_bytes(fingerprint, item.name.as_bytes());
    hash_bytes(fingerprint, item.description.as_bytes());
    fingerprint.update(item.role.to_le_bytes());
    fingerprint.update(
        u64::try_from(item.states.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for state in &item.states {
        fingerprint.update(state.to_le_bytes());
    }
    fingerprint.update(
        u64::try_from(children.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for child in children {
        hash_address(fingerprint, child);
    }
}

fn hash_strings(fingerprint: &mut Sha256, values: &[String]) {
    fingerprint.update(
        u64::try_from(values.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for value in values {
        hash_bytes(fingerprint, value.as_bytes());
    }
}

fn hash_address(fingerprint: &mut Sha256, address: &ObjectAddress) {
    hash_bytes(fingerprint, address.bus_name().as_bytes());
    hash_bytes(fingerprint, address.object_path().as_bytes());
}

fn hash_usize(fingerprint: &mut Sha256, value: Option<usize>) {
    fingerprint.update(
        value
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
}

fn hash_bytes(fingerprint: &mut Sha256, value: &[u8]) {
    fingerprint.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    fingerprint.update(value);
}

fn validate_lazy_node(
    object: &ObjectAddress,
    application_root: &ObjectAddress,
    expected_parent: Option<&ObjectAddress>,
    expected_index: Option<usize>,
    node: &LazyAccessibleNode,
    limits: CacheLimits,
) -> Result<(), BackendFailure> {
    let external_application_parent =
        application_root_has_external_parent(object, application_root, node.parent.as_ref());
    let actual_parent = if external_application_parent {
        None
    } else {
        node.parent.as_ref()
    };
    if object.bus_name() != application_root.bus_name()
        || node.application != *application_root
        || actual_parent != expected_parent
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility ownership or parent provenance mismatch",
        ));
    }
    let actual_index = if external_application_parent {
        None
    } else {
        usize::try_from(node.index_in_parent).ok()
    };
    if actual_index != expected_index {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility child index mismatch",
        ));
    }
    let child_count = usize::try_from(node.child_count).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility child count was negative",
        )
    })?;
    if child_count != node.children.len() {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility child count contradicted GetChildren",
        ));
    }
    if node.children.len() > limits.max_children {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility per-node child limit exceeded",
        ));
    }
    if node.children.iter().any(|child| {
        child.bus_name() != application_root.bus_name()
            || child.object_path() == "/org/a11y/atspi/null"
    }) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy accessibility child ownership provenance mismatch",
        ));
    }
    Ok(())
}

impl AtspiBackend for LiveAtspiBackend {
    fn bootstrap(
        &mut self,
        limits: CacheLimits,
        proxy_call_timeout: std::time::Duration,
    ) -> BackendFuture<'_, Result<Vec<NormalizedCacheItem>, BackendFailure>> {
        Box::pin(async move {
            let registry_root = timeout(
                proxy_call_timeout,
                self.connection.root_accessible_on_registry(),
            )
            .await
            .map_err(|_| {
                BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "registry root lookup timed out",
                )
            })?
            .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            let applications = timeout(proxy_call_timeout, registry_root.get_children())
                .await
                .map_err(|_| {
                    BackendFailure::new(
                        BackendFailureKind::Timeout,
                        "registry children lookup timed out",
                    )
                })?
                .map_err(|error| backend_error(BackendFailureKind::Connection, error))?;
            if applications.len() > limits.max_nodes {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    format!(
                        "registry application root limit exceeded: {} > {}",
                        applications.len(),
                        limits.max_nodes
                    ),
                ));
            }

            let mut items = Vec::new();
            let mut aggregate_bytes = 0_usize;
            let mut barriers = BootstrapBarriers::new();
            let mut degraded = false;
            for application in applications {
                let Some(bus_name) = application.name_as_str() else {
                    continue;
                };
                let application_cache = fetch_application_cache_with_retry(
                    self.connection.connection(),
                    bus_name,
                    limits,
                    proxy_call_timeout,
                )
                .await;
                match application_cache {
                    Ok((mut application_items, position)) => {
                        let total = items.len().saturating_add(application_items.len());
                        if total > limits.max_nodes {
                            return Err(BackendFailure::new(
                                BackendFailureKind::Protocol,
                                format!(
                                    "aggregate Cache.GetItems node limit exceeded: {total} > {}",
                                    limits.max_nodes
                                ),
                            ));
                        }
                        aggregate_bytes =
                            account_bootstrap_items(aggregate_bytes, &application_items, limits)?;
                        items.append(&mut application_items);
                        barriers.insert(
                            bus_name.to_owned(),
                            ApplicationBootstrapBarrier::Ready(position),
                        );
                    }
                    Err(_) => {
                        let application_root =
                            ObjectAddress::new(bus_name, application.path_as_str()).map_err(
                                |error| backend_error(BackendFailureKind::Protocol, error),
                            )?;
                        let lazy_result = traverse_lazy_application(
                            &mut LiveLazyAccessibleSource {
                                connection: self.connection.connection(),
                            },
                            application_root.clone(),
                            limits,
                            proxy_call_timeout,
                        )
                        .await;
                        match lazy_result {
                            Ok(mut application_items) => {
                                let marker = lazy_traversal_marker(
                                    self.connection.connection(),
                                    &application_root,
                                    proxy_call_timeout,
                                )
                                .await?;
                                let total = items.len().saturating_add(application_items.len());
                                if total > limits.max_nodes {
                                    return Err(BackendFailure::new(
                                        BackendFailureKind::Protocol,
                                        "aggregate lazy accessibility node limit exceeded",
                                    ));
                                }
                                aggregate_bytes = account_bootstrap_items(
                                    aggregate_bytes,
                                    &application_items,
                                    limits,
                                )?;
                                items.append(&mut application_items);
                                barriers.insert(
                                    bus_name.to_owned(),
                                    ApplicationBootstrapBarrier::LazyReady(marker),
                                );
                            }
                            Err(_) => {
                                let fallback = degraded_application_root(
                                    bus_name,
                                    application.path_as_str(),
                                    limits,
                                )?;
                                if items.len() == limits.max_nodes {
                                    return Err(BackendFailure::new(
                                        BackendFailureKind::Protocol,
                                        "degraded application root exceeded cache node limit",
                                    ));
                                }
                                aggregate_bytes = account_bootstrap_items(
                                    aggregate_bytes,
                                    std::slice::from_ref(&fallback),
                                    limits,
                                )?;
                                items.push(fallback);
                                barriers.insert(
                                    bus_name.to_owned(),
                                    ApplicationBootstrapBarrier::Degraded,
                                );
                                degraded = true;
                                let _result =
                                    self.ingress.offer(BackendEvent::ApplicationDegraded {
                                        bus_name: bus_name.to_owned(),
                                        reason: "cache_and_lazy_traversal_unavailable_or_malformed",
                                    });
                            }
                        }
                    }
                }
            }
            self.bootstrap_barriers
                .send_replace(Some(Arc::new(barriers)));
            timeout(
                proxy_call_timeout,
                self.bootstrap_barriers_applied.wait_for(|applied| *applied),
            )
            .await
            .map_err(|_| {
                BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "cache signal barrier installation timed out",
                )
            })?
            .map_err(|_| {
                BackendFailure::new(
                    BackendFailureKind::Stream,
                    "cache signal drain ended before barrier installation",
                )
            })?;
            if degraded {
                let ingress = self.ingress.clone();
                let cancellation = self.drain_cancellation.clone();
                self.degraded_retry = Some(tokio::spawn(async move {
                    tokio::select! {
                        () = cancellation.cancelled() => {}
                        () = tokio::time::sleep(DEGRADED_RETRY_DELAY) => {
                            let _result = ingress.offer(BackendEvent::ResyncRequired {
                                reason: "degraded_application_retry",
                            });
                        }
                    }
                }));
            }
            Ok(items)
        })
    }

    fn execute_semantic(
        &mut self,
        request: BackendSemanticRequest,
        dispatch: SemanticDispatchMarker,
    ) -> BackendFuture<'_, Result<SemanticEvidence, BackendFailure>> {
        Box::pin(execute_live_semantic(
            self.connection.connection(),
            request,
            dispatch,
        ))
    }

    fn observe_exact(
        &mut self,
        request: BackendObservationRequest,
    ) -> BackendFuture<'_, Result<SemanticObservationEvidence, BackendFailure>> {
        Box::pin(observe_live_exact(self.connection.connection(), request))
    }

    fn refresh_object(
        &mut self,
        request: BackendRefreshRequest,
    ) -> BackendFuture<'_, Result<RefreshedCacheItem, BackendFailure>> {
        Box::pin(refresh_live_object(self.connection.connection(), request))
    }

    fn shutdown(&mut self) -> BackendFuture<'_, ()> {
        Box::pin(async move {
            let _ignored = self.connection.remove_registry_event::<CacheEvents>().await;
            let _ignored = self
                .connection
                .remove_registry_event::<ObjectEvents>()
                .await;
            let _ignored = self.connection.remove_registry_event::<FocusEvents>().await;
            let _ignored = self
                .connection
                .remove_registry_event::<WindowEvents>()
                .await;
            self.drain_cancellation.cancel();
            if let Some(retry) = self.degraded_retry.take() {
                let _ignored = retry.await;
            }
            if let Some(drain) = self.drain.take() {
                let _ignored = drain.await;
            }
        })
    }
}

fn account_bootstrap_items(
    current: usize,
    items: &[NormalizedCacheItem],
    limits: CacheLimits,
) -> Result<usize, BackendFailure> {
    let application_bytes = items.iter().try_fold(0_usize, |total, item| {
        total.checked_add(item.estimated_bytes()).ok_or_else(|| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "aggregate Cache.GetItems byte accounting overflowed",
            )
        })
    })?;
    let aggregate = current.checked_add(application_bytes).ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "aggregate Cache.GetItems byte accounting overflowed",
        )
    })?;
    if aggregate > limits.max_total_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "aggregate Cache.GetItems exceeded total cache byte limit",
        ));
    }
    Ok(aggregate)
}

impl Drop for LiveAtspiBackend {
    fn drop(&mut self) {
        self.drain_cancellation.cancel();
    }
}

async fn execute_live_semantic(
    connection: &zbus::Connection,
    request: BackendSemanticRequest,
    dispatch: SemanticDispatchMarker,
) -> Result<SemanticEvidence, BackendFailure> {
    let dispatch_permit = request.dispatch_permit;
    let terminal_deadline = request.deadline;
    let expected_child_count = request.expected_child_count;
    let identity = LiveSemanticIdentity {
        object: request.object,
        application: request.application,
        expected_identity: request.expected_identity,
        expected_index_in_parent: request.expected_index_in_parent,
        expected_role: request.expected_role,
        expected_states: request.expected_states,
        proxy_call_timeout: request.proxy_call_timeout,
        cache_limits: request.cache_limits,
    };
    let bus_name = identity.object.bus_name();
    let object_path = identity.object.object_path();
    let call_timeout = identity.proxy_call_timeout;
    match request.operation {
        SemanticOperation::Invoke(selector) => {
            let proxy = timed_call(
                call_timeout,
                "Action proxy construction",
                ActionProxy::builder(connection)
                    .cache_properties(CacheProperties::No)
                    .destination(bus_name)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .path(object_path)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .build(),
            )
            .await?;
            let actions = read_actions(
                connection,
                &identity.object,
                call_timeout,
                identity.cache_limits.max_item_bytes,
            )
            .await?;
            validate_actions(&actions)?;
            let index = resolve_action(&actions, &selector)?;
            revalidate_live_identity(connection, &identity, false).await?;
            let immediate_actions = read_actions(
                connection,
                &identity.object,
                call_timeout,
                identity.cache_limits.max_item_bytes,
            )
            .await?;
            validate_actions(&immediate_actions)?;
            let immediate_index = resolve_action(&immediate_actions, &selector)?;
            if immediate_index != index || immediate_actions != actions {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "live action metadata changed during semantic preflight",
                ));
            }
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let accepted = timed_call(
                call_timeout,
                "Action.DoAction",
                proxy.do_action(i32::try_from(index).map_err(|_| {
                    BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
                })?),
            )
            .await?;
            Ok(SemanticEvidence::Action {
                accepted,
                invoked_index: index,
                actions: action_evidence(immediate_actions)?,
            })
        }
        SemanticOperation::Focus => {
            let proxy = timed_call(
                call_timeout,
                "Component proxy construction",
                ComponentProxy::builder(connection)
                    .cache_properties(CacheProperties::No)
                    .destination(bus_name)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .path(object_path)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .build(),
            )
            .await?;
            revalidate_live_identity(connection, &identity, false).await?;
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let accepted =
                timed_call(call_timeout, "Component.GrabFocus", proxy.grab_focus()).await?;
            let focused = if accepted {
                wait_for_raw_state(
                    connection,
                    &identity.object,
                    12,
                    terminal_deadline,
                    call_timeout,
                    identity.cache_limits.max_item_bytes,
                    identity.cache_limits.max_states,
                )
                .await?
            } else {
                false
            };
            Ok(SemanticEvidence::Focus { accepted, focused })
        }
        SemanticOperation::SetValue(value) => {
            let proxy = timed_call(
                call_timeout,
                "Value proxy construction",
                ValueProxy::builder(connection)
                    .cache_properties(CacheProperties::No)
                    .destination(bus_name)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .path(object_path)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .build(),
            )
            .await?;
            let minimum =
                timed_call(call_timeout, "Value.MinimumValue", proxy.minimum_value()).await?;
            let maximum =
                timed_call(call_timeout, "Value.MaximumValue", proxy.maximum_value()).await?;
            if !minimum.is_finite() || !maximum.is_finite() || value < minimum || value > maximum {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "requested value is outside finite live bounds",
                ));
            }
            revalidate_live_identity(connection, &identity, false).await?;
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            timed_call(
                call_timeout,
                "Value.SetCurrentValue",
                proxy.set_current_value(value),
            )
            .await?;
            let value_evidence =
                settle_value_evidence(&proxy, value, terminal_deadline, call_timeout).await?;
            Ok(SemanticEvidence::Value {
                current: value_evidence.current,
                minimum: value_evidence.minimum,
                maximum: value_evidence.maximum,
                minimum_increment: value_evidence.minimum_increment,
            })
        }
        SemanticOperation::Selection(operation) => {
            let proxy = timed_call(
                call_timeout,
                "Selection proxy construction",
                SelectionProxy::builder(connection)
                    .cache_properties(CacheProperties::No)
                    .destination(bus_name)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .path(object_path)
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .build(),
            )
            .await?;
            revalidate_live_identity(connection, &identity, false).await?;
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let accepted = match operation {
                SelectionOperation::Clear => {
                    timed_call(
                        call_timeout,
                        "Selection.ClearSelection",
                        proxy.clear_selection(),
                    )
                    .await?
                }
                SelectionOperation::SelectChild(index) => {
                    timed_call(
                        call_timeout,
                        "Selection.SelectChild",
                        proxy.select_child(index_to_i32(index)?),
                    )
                    .await?
                }
                SelectionOperation::DeselectChild(index) => {
                    timed_call(
                        call_timeout,
                        "Selection.DeselectChild",
                        proxy.deselect_child(index_to_i32(index)?),
                    )
                    .await?
                }
                SelectionOperation::SelectAll => {
                    timed_call(call_timeout, "Selection.SelectAll", proxy.select_all()).await?
                }
            };
            let (selected_children, addressed_child_selected) = settle_selection_evidence(
                &proxy,
                operation,
                accepted,
                expected_child_count,
                terminal_deadline,
                call_timeout,
            )
            .await?;
            Ok(SemanticEvidence::Selection {
                accepted,
                selected_children,
                addressed_child_selected,
            })
        }
        SemanticOperation::SetText {
            text,
            selection,
            verification,
        } => {
            execute_text_write(
                connection,
                &identity,
                dispatch,
                &dispatch_permit,
                LiveTextWrite {
                    position: None,
                    text,
                    selection_policy: selection,
                    verification,
                },
                terminal_deadline,
            )
            .await
        }
        SemanticOperation::InsertText {
            position,
            text,
            selection,
            verification,
        } => {
            execute_text_write(
                connection,
                &identity,
                dispatch,
                &dispatch_permit,
                LiveTextWrite {
                    position: Some(position),
                    text,
                    selection_policy: selection,
                    verification,
                },
                terminal_deadline,
            )
            .await
        }
        SemanticOperation::Scroll(placement) => {
            let proxy = component_proxy(connection, &identity.object, call_timeout).await?;
            let before = read_extents(&proxy, call_timeout).await?;
            revalidate_live_identity(connection, &identity, false).await?;
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let accepted = timed_call(
                call_timeout,
                "Component.ScrollTo",
                proxy.scroll_to(scroll_type(placement)),
            )
            .await?;
            let after = settle_scroll_extents(
                &proxy,
                before,
                None,
                accepted,
                terminal_deadline,
                call_timeout,
            )
            .await?;
            Ok(SemanticEvidence::Scroll {
                accepted,
                before,
                after,
            })
        }
        SemanticOperation::ScrollToPoint { x, y } => {
            let proxy = component_proxy(connection, &identity.object, call_timeout).await?;
            let before = read_extents(&proxy, call_timeout).await?;
            revalidate_live_identity(connection, &identity, false).await?;
            dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let accepted = timed_call(
                call_timeout,
                "Component.ScrollToPoint",
                proxy.scroll_to_point(CoordType::Screen, x, y),
            )
            .await?;
            let after = settle_scroll_extents(
                &proxy,
                before,
                Some((x, y)),
                accepted,
                terminal_deadline,
                call_timeout,
            )
            .await?;
            Ok(SemanticEvidence::Scroll {
                accepted,
                before,
                after,
            })
        }
    }
}

async fn observe_live_exact(
    connection: &zbus::Connection,
    request: BackendObservationRequest,
) -> Result<SemanticObservationEvidence, BackendFailure> {
    let application = raw_required_object_ref(
        &bounded_accessible_empty_reply(
            connection,
            &request.object,
            "GetApplication",
            request.proxy_call_timeout,
            request.cache_limits.max_item_bytes,
        )
        .await?,
    )?;
    if application != request.application {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "fresh observation application identity changed",
        ));
    }
    let mut parent = raw_optional_object_property(
        connection,
        &request.object,
        "Parent",
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
    )
    .await?;
    if application_root_has_external_parent(&request.object, &application, parent.as_ref()) {
        parent = None;
    }
    let raw_index = bounded_accessible_empty_reply(
        connection,
        &request.object,
        "GetIndexInParent",
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
    )
    .await?
    .body()
    .deserialize::<i32>()
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let name = raw_string_property(
        connection,
        &request.object,
        "Name",
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
        request.cache_limits.max_string_bytes,
    )
    .await?;
    let description = raw_string_property(
        connection,
        &request.object,
        "Description",
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
        request.cache_limits.max_string_bytes,
    )
    .await?;
    let interfaces = read_raw_interfaces(
        connection,
        &request.object,
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
        request.cache_limits.max_interfaces,
    )
    .await?;
    let role = read_raw_role(
        connection,
        &request.object,
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
    )
    .await?;
    let states = read_raw_states(
        connection,
        &request.object,
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
        request.cache_limits.max_states,
    )
    .await?;
    let normalized = normalize_modern(
        ModernCacheItem {
            object: request.object.clone(),
            application: application.clone(),
            parent: parent.clone(),
            index_in_parent: raw_index,
            child_count: 0,
            interfaces,
            short_name: name,
            role,
            name: description,
            states,
        },
        request.cache_limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if normalized.identity_fingerprint() != request.expected_identity
        || normalized.index_in_parent != request.expected_index_in_parent
        || normalized.role != request.expected_role
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "fresh observation target identity changed",
        ));
    }
    let live = read_live_metadata(
        connection,
        &request.object,
        &normalized,
        request.proxy_call_timeout,
        request.cache_limits,
    )
    .await?;
    let top_level = find_top_level(
        connection,
        &request.object,
        &application,
        parent,
        request.proxy_call_timeout,
        request.cache_limits,
    )
    .await?;
    let application_pid = read_application_pid(
        connection,
        application.bus_name(),
        request.proxy_call_timeout,
        request.cache_limits.max_item_bytes,
    )
    .await;
    request.read_permit.ensure_current()?;
    let text = live.text.and_then(|text| {
        Some(TextReadbackEvidence {
            character_count: text.character_count?,
            caret_offset: text.caret_offset?,
            selections: text.selections,
        })
    });
    Ok(SemanticObservationEvidence {
        identity_fingerprint: normalized.identity_fingerprint(),
        parent: normalized.parent,
        index_in_parent: normalized.index_in_parent,
        role: normalized.role,
        states: normalized.states,
        interfaces: normalized.interfaces,
        bounds: live.bounds,
        top_level,
        application_pid,
        value: live.value,
        text,
        selected_children: live.selected_children,
    })
}

async fn refresh_live_object(
    connection: &zbus::Connection,
    request: BackendRefreshRequest,
) -> Result<RefreshedCacheItem, BackendFailure> {
    // Targeted metadata hydration deliberately does not enumerate children.
    // Chromium and other dynamic toolkits may replace descendants between
    // ChildCount and GetChildAtIndex even though this exact object remains
    // valid. Full child topology is verified by the two-pass bootstrap lane.
    let node = read_live_lazy_node(
        connection,
        &request.object,
        request.timeout,
        request.cache_limits,
        false,
    )
    .await
    .map_err(|failure| {
        BackendFailure::new(
            failure.kind,
            "targeted refresh common accessible metadata failed",
        )
    })?;
    if node.application != request.expected_application {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "targeted refresh application identity changed",
        ));
    }
    let item = normalize_modern(
        ModernCacheItem {
            object: request.object.clone(),
            application: node.application,
            parent: node.parent,
            index_in_parent: node.index_in_parent,
            child_count: node.child_count,
            interfaces: node.interfaces,
            short_name: node.name,
            role: node.role,
            name: node.description,
            states: node.states,
        },
        request.cache_limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let live = read_live_metadata(
        connection,
        &request.object,
        &item,
        request.timeout,
        request.cache_limits,
    )
    .await?;
    Ok(RefreshedCacheItem { item, live })
}

async fn read_live_metadata(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    item: &NormalizedCacheItem,
    call_timeout: Duration,
    _limits: CacheLimits,
) -> Result<CachedLiveMetadata, BackendFailure> {
    let has_interface = |name: &str| item.interfaces.iter().any(|candidate| candidate == name);
    let bounds = if has_interface("org.a11y.atspi.Component") {
        let component = component_proxy(connection, object, call_timeout)
            .await
            .map_err(|failure| {
                BackendFailure::new(failure.kind, "targeted refresh Component metadata failed")
            })?;
        Some(
            read_extents(&component, call_timeout)
                .await
                .map_err(|failure| {
                    BackendFailure::new(failure.kind, "targeted refresh Component metadata failed")
                })?,
        )
    } else {
        None
    };
    let value = if has_interface("org.a11y.atspi.Value") {
        let proxy = timed_call(
            call_timeout,
            "refresh Value proxy construction",
            ValueProxy::builder(connection)
                .cache_properties(CacheProperties::No)
                .destination(object.bus_name())
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .path(object.object_path())
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .build(),
        )
        .await?;
        let evidence = SemanticValueEvidence {
            current: timed_call(call_timeout, "Value.CurrentValue", proxy.current_value()).await?,
            minimum: timed_call(call_timeout, "Value.MinimumValue", proxy.minimum_value()).await?,
            maximum: timed_call(call_timeout, "Value.MaximumValue", proxy.maximum_value()).await?,
            minimum_increment: timed_call(
                call_timeout,
                "Value.MinimumIncrement",
                proxy.minimum_increment(),
            )
            .await?,
        };
        if !evidence.current.is_finite()
            || !evidence.minimum.is_finite()
            || !evidence.maximum.is_finite()
            || !evidence.minimum_increment.is_finite()
        {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Value metadata contained a non-finite number",
            ));
        }
        Some(evidence)
    } else {
        None
    };
    let text = if has_interface("org.a11y.atspi.Text") {
        if item.text_protection == crate::TextProtection::Unknown {
            // A future role may alter the sensitivity of even content-free
            // metrics. Retain nothing until that role has been classified.
            Some(CachedTextMetadata::default())
        } else {
            let proxy = timed_call(
                call_timeout,
                "refresh Text proxy construction",
                TextProxy::builder(connection)
                    .cache_properties(CacheProperties::No)
                    .destination(object.bus_name())
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .path(object.object_path())
                    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                    .build(),
            )
            .await?;
            let evidence = read_text_evidence(&proxy, call_timeout).await?;
            // Character count, caret, and selection offsets do not contain or
            // retrieve text content and are the only protected metadata kept.
            Some(CachedTextMetadata {
                character_count: Some(evidence.character_count),
                caret_offset: Some(evidence.caret_offset),
                selections: evidence.selections,
            })
        }
    } else {
        None
    };
    let selected_children = if has_interface("org.a11y.atspi.Selection") {
        let proxy = timed_call(
            call_timeout,
            "refresh Selection proxy construction",
            SelectionProxy::builder(connection)
                .cache_properties(CacheProperties::No)
                .destination(object.bus_name())
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .path(object.object_path())
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
                .build(),
        )
        .await?;
        let count = timed_call(
            call_timeout,
            "Selection.GetNSelectedChildren",
            proxy.n_selected_children(),
        )
        .await?;
        Some(u32::try_from(count).map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "Selection returned a negative selected-child count",
            )
        })?)
    } else {
        None
    };
    Ok(CachedLiveMetadata {
        bounds,
        value,
        text,
        selected_children,
    })
}

async fn find_top_level(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    application: &ObjectAddress,
    mut parent: Option<ObjectAddress>,
    call_timeout: Duration,
    limits: CacheLimits,
) -> Result<Option<ObjectAddress>, BackendFailure> {
    if object == application {
        return Ok(None);
    }
    let mut current = object.clone();
    let mut visited = BTreeSet::from([current.clone()]);
    for _depth in 0..MAX_LAZY_TRAVERSAL_DEPTH {
        let candidate = parent.ok_or_else(|| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "fresh observation ancestor chain ended before the application root",
            )
        })?;
        if candidate == *application {
            return Ok(Some(current));
        }
        if candidate.bus_name() != application.bus_name() || !visited.insert(candidate.clone()) {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "fresh observation ancestor ownership or cycle violation",
            ));
        }
        let ancestor_application = raw_required_object_ref(
            &bounded_accessible_empty_reply(
                connection,
                &candidate,
                "GetApplication",
                call_timeout,
                limits.max_item_bytes,
            )
            .await?,
        )?;
        if ancestor_application != *application {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "fresh observation ancestor application changed",
            ));
        }
        parent = raw_optional_object_property(
            connection,
            &candidate,
            "Parent",
            call_timeout,
            limits.max_item_bytes,
        )
        .await?;
        current = candidate;
    }
    Err(BackendFailure::new(
        BackendFailureKind::Protocol,
        "fresh observation ancestor depth limit exceeded",
    ))
}

async fn read_application_pid(
    connection: &zbus::Connection,
    bus_name: &str,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Option<u32> {
    let reply = timeout(
        call_timeout,
        connection.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(bus_name,),
        ),
    )
    .await
    .ok()?
    .ok()?;
    validate_reply_bytes(&reply, max_reply_bytes).ok()?;
    reply.body().deserialize::<u32>().ok()
}

async fn read_value_evidence(
    proxy: &ValueProxy<'_>,
    call_timeout: Duration,
) -> Result<SemanticValueEvidence, BackendFailure> {
    let evidence = SemanticValueEvidence {
        current: timed_call(call_timeout, "Value.CurrentValue", proxy.current_value()).await?,
        minimum: timed_call(call_timeout, "Value.MinimumValue", proxy.minimum_value()).await?,
        maximum: timed_call(call_timeout, "Value.MaximumValue", proxy.maximum_value()).await?,
        minimum_increment: timed_call(
            call_timeout,
            "Value.MinimumIncrement",
            proxy.minimum_increment(),
        )
        .await?,
    };
    if [
        evidence.current,
        evidence.minimum,
        evidence.maximum,
        evidence.minimum_increment,
    ]
    .iter()
    .any(|value| !value.is_finite())
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Value readback contained a non-finite number",
        ));
    }
    Ok(evidence)
}

fn semantic_value_reached(current: f64, requested: f64) -> bool {
    let normalized_bits = |value: f64| {
        let bits = value.to_bits();
        if bits << 1 == 0 { 0 } else { bits }
    };
    normalized_bits(current) == normalized_bits(requested)
}

async fn settle_value_evidence(
    proxy: &ValueProxy<'_>,
    requested: f64,
    terminal_deadline: Instant,
    settle_budget: Duration,
) -> Result<SemanticValueEvidence, BackendFailure> {
    let mut settle = SemanticSettle::new(terminal_deadline, settle_budget);
    let mut last = None;
    let mut last_timeout = None;
    while let Some(call_timeout) = settle.next_call_timeout(settle_budget) {
        match timeout(call_timeout, read_value_evidence(proxy, call_timeout)).await {
            Ok(Ok(evidence)) => {
                let reached = semantic_value_reached(evidence.current, requested);
                last = Some(evidence);
                if reached {
                    break;
                }
            }
            Ok(Err(error)) if error.kind == BackendFailureKind::Timeout => {
                last_timeout = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                last_timeout = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "Value settling read timed out",
                ));
            }
        }
        if !settle.pause().await {
            break;
        }
    }
    finish_semantic_settle(
        last,
        last_timeout,
        "Value settling produced no valid readback",
    )
}

async fn read_selection_evidence(
    proxy: &SelectionProxy<'_>,
    operation: SelectionOperation,
    call_timeout: Duration,
) -> Result<(u32, Option<bool>), BackendFailure> {
    let selected_children = timed_call(
        call_timeout,
        "Selection.NSelectedChildren",
        proxy.n_selected_children(),
    )
    .await?;
    let selected_children = u32::try_from(selected_children).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Selection returned a negative selected-child count",
        )
    })?;
    let addressed_child_selected = match operation {
        SelectionOperation::SelectChild(index) | SelectionOperation::DeselectChild(index) => Some(
            timed_call(
                call_timeout,
                "Selection.IsChildSelected",
                proxy.is_child_selected(index_to_i32(index)?),
            )
            .await?,
        ),
        SelectionOperation::Clear | SelectionOperation::SelectAll => None,
    };
    Ok((selected_children, addressed_child_selected))
}

fn selection_readback_reached(
    operation: SelectionOperation,
    selected_children: u32,
    addressed_child_selected: Option<bool>,
    expected_child_count: Option<u32>,
) -> bool {
    match operation {
        SelectionOperation::Clear => selected_children == 0,
        SelectionOperation::SelectChild(_) => addressed_child_selected == Some(true),
        SelectionOperation::DeselectChild(_) => addressed_child_selected == Some(false),
        SelectionOperation::SelectAll => expected_child_count == Some(selected_children),
    }
}

async fn settle_selection_evidence(
    proxy: &SelectionProxy<'_>,
    operation: SelectionOperation,
    accepted: bool,
    expected_child_count: Option<usize>,
    terminal_deadline: Instant,
    settle_budget: Duration,
) -> Result<(u32, Option<bool>), BackendFailure> {
    let expected_child_count = expected_child_count
        .map(u32::try_from)
        .transpose()
        .map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "selection child count exceeds AT-SPI's unsigned range",
            )
        })?;
    let mut settle = SemanticSettle::new(terminal_deadline, settle_budget);
    let mut last = None;
    let mut last_timeout = None;
    while let Some(call_timeout) = settle.next_call_timeout(settle_budget) {
        match timeout(
            call_timeout,
            read_selection_evidence(proxy, operation, call_timeout),
        )
        .await
        {
            Ok(Ok(evidence)) => {
                let reached = !accepted
                    || selection_readback_reached(
                        operation,
                        evidence.0,
                        evidence.1,
                        expected_child_count,
                    );
                last = Some(evidence);
                if reached {
                    break;
                }
            }
            Ok(Err(error)) if error.kind == BackendFailureKind::Timeout => {
                last_timeout = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                last_timeout = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "Selection settling read timed out",
                ));
            }
        }
        if !settle.pause().await {
            break;
        }
    }
    finish_semantic_settle(
        last,
        last_timeout,
        "Selection settling produced no valid readback",
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TextSettleExpectation {
    CharacterCount(u32),
    Exact(TextReadbackEvidence),
}

#[derive(Debug)]
struct LiveTextWrite {
    position: Option<TextInsertPosition>,
    text: crate::semantic::RedactedText,
    selection_policy: TextSelectionPolicy,
    verification: TextVerificationMode,
}

impl TextSettleExpectation {
    fn reached(&self, evidence: &TextReadbackEvidence) -> bool {
        match self {
            Self::CharacterCount(expected) => evidence.character_count == *expected,
            Self::Exact(expected) => evidence == expected,
        }
    }
}

async fn settle_text_evidence(
    proxy: &TextProxy<'_>,
    expectation: TextSettleExpectation,
    terminal_deadline: Instant,
    settle_budget: Duration,
) -> Result<TextReadbackEvidence, BackendFailure> {
    let mut settle = SemanticSettle::new(terminal_deadline, settle_budget);
    let mut last = None;
    let mut last_timeout = None;
    while let Some(call_timeout) = settle.next_call_timeout(settle_budget) {
        match timeout(call_timeout, read_text_evidence(proxy, call_timeout)).await {
            Ok(Ok(evidence)) => {
                let reached = expectation.reached(&evidence);
                last = Some(evidence);
                if reached {
                    break;
                }
            }
            Ok(Err(error)) if error.kind == BackendFailureKind::Timeout => {
                last_timeout = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                last_timeout = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "Text settling read timed out",
                ));
            }
        }
        if !settle.pause().await {
            break;
        }
    }
    finish_semantic_settle(
        last,
        last_timeout,
        "Text settling produced no valid readback",
    )
}

async fn execute_text_write(
    connection: &zbus::Connection,
    identity: &LiveSemanticIdentity,
    dispatch: SemanticDispatchMarker,
    dispatch_permit: &crate::semantic::SemanticDispatchPermit,
    operation: LiveTextWrite,
    terminal_deadline: Instant,
) -> Result<SemanticEvidence, BackendFailure> {
    let LiveTextWrite {
        position,
        text,
        selection_policy,
        verification,
    } = operation;
    let call_timeout = identity.proxy_call_timeout;
    let editable = timed_call(
        call_timeout,
        "EditableText proxy construction",
        EditableTextProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(identity.object.bus_name())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .path(identity.object.object_path())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .build(),
    )
    .await?;
    let text_proxy = timed_call(
        call_timeout,
        "Text proxy construction",
        TextProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(identity.object.bus_name())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .path(identity.object.object_path())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .build(),
    )
    .await?;
    let before = read_text_evidence(&text_proxy, call_timeout).await?;
    let insertion_start = match position {
        Some(TextInsertPosition::Offset(position)) => position,
        Some(TextInsertPosition::LiveCaret) => {
            u32::try_from(before.caret_offset).map_err(|_| {
                BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "editable-text target reported no live caret",
                )
            })?
        }
        None => 0,
    };
    if insertion_start > before.character_count {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "editable-text insertion offset exceeds live character count",
        ));
    }
    let inserted_characters = u32::try_from(text.character_len()).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "editable-text character length overflow",
        )
    })?;
    let insertion_end = insertion_start
        .checked_add(inserted_characters)
        .ok_or_else(|| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "editable-text insertion range overflow",
            )
        })?;
    revalidate_live_identity(connection, identity, true).await?;
    let immediate = read_text_evidence(&text_proxy, call_timeout).await?;
    if immediate != before {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live text caret, count, or selection changed during semantic preflight",
        ));
    }
    dispatch_permit.ensure_current()?;
    dispatch.mark_dispatched();
    let accepted = if position.is_some() {
        timed_secret_call(
            call_timeout,
            "EditableText.InsertText",
            editable.insert_text(
                index_to_i32(insertion_start)?,
                text.expose_to_backend(),
                text.character_len(),
            ),
        )
        .await?
    } else {
        timed_secret_call(
            call_timeout,
            "EditableText.SetTextContents",
            editable.set_text_contents(text.expose_to_backend()),
        )
        .await?
    };
    let expected_character_count = if position.is_some() {
        before
            .character_count
            .checked_add(inserted_characters)
            .ok_or_else(|| {
                BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "editable-text character-count readback overflow",
                )
            })?
    } else {
        inserted_characters
    };
    let post_write = settle_text_evidence(
        &text_proxy,
        TextSettleExpectation::CharacterCount(expected_character_count),
        terminal_deadline,
        call_timeout,
    )
    .await?;
    let after = if accepted && post_write.character_count == expected_character_count {
        let desired = apply_text_selection_policy(
            &text_proxy,
            &before,
            &post_write,
            insertion_start,
            insertion_end,
            selection_policy,
            call_timeout,
        )
        .await?;
        settle_text_evidence(
            &text_proxy,
            TextSettleExpectation::Exact(desired),
            terminal_deadline,
            call_timeout,
        )
        .await?
    } else {
        // A stale or permanently mismatched length must never drive caret or
        // selection mutations. Return the last valid observation so the daemon
        // can report a deterministic postcondition failure after one dispatch.
        post_write
    };
    let exact_match = if !exact_text_readback_required(verification) {
        None
    } else if accepted && after.character_count == expected_character_count {
        Some(
            read_exact_text_match(
                &text_proxy,
                position,
                insertion_start,
                &text,
                terminal_deadline,
                call_timeout,
            )
            .await?,
        )
    } else {
        Some(false)
    };
    Ok(SemanticEvidence::Text {
        accepted,
        before,
        after,
        exact_match,
    })
}

const fn exact_text_readback_required(verification: TextVerificationMode) -> bool {
    match verification {
        TextVerificationMode::LengthOnly => false,
        TextVerificationMode::Exact => true,
    }
}

fn exact_text_readback_range(
    position: Option<TextInsertPosition>,
    insertion_start: u32,
    inserted_characters: u32,
) -> Result<(i32, i32), BackendFailure> {
    let start = if position.is_some() {
        insertion_start
    } else {
        0
    };
    let end = start.checked_add(inserted_characters).ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "exact text verification range overflow",
        )
    })?;
    Ok((index_to_i32(start)?, index_to_i32(end)?))
}

fn exact_text_matches(
    requested: &crate::semantic::RedactedText,
    observed: &str,
) -> Result<bool, BackendFailure> {
    if observed.len() > crate::semantic::MAX_SEMANTIC_TEXT_BYTES {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "exact text verification readback exceeds the adapter byte limit",
        ));
    }
    let observed_characters = u32::try_from(observed.chars().count()).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "exact text verification readback exceeds the adapter character limit",
        )
    })?;
    Ok(observed_characters == requested.character_count()
        && observed == requested.expose_to_backend())
}

fn decode_bounded_exact_text_reply<'data, 'bytes, 'fds>(
    signature: &zbus::zvariant::Signature,
    data: &'data zbus::zvariant::serialized::Data<'bytes, 'fds>,
) -> Result<&'data str, BackendFailure> {
    if data.len() > MAX_EXACT_TEXT_REPLY_BODY_BYTES {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.GetText reply exceeds the exact-verification byte limit",
        ));
    }
    if signature != &zbus::zvariant::Signature::Str {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.GetText returned an unexpected reply signature",
        ));
    }
    let (observed, consumed) = data.deserialize::<&str>().map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.GetText returned a malformed string reply",
        )
    })?;
    if consumed != data.len() {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.GetText returned trailing reply content",
        ));
    }
    if observed.as_bytes().contains(&0) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.GetText returned an embedded NUL",
        ));
    }
    if observed.len() > crate::semantic::MAX_SEMANTIC_TEXT_BYTES {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "exact text verification readback exceeds the adapter byte limit",
        ));
    }
    Ok(observed)
}

async fn read_exact_text_match(
    text_proxy: &TextProxy<'_>,
    position: Option<TextInsertPosition>,
    insertion_start: u32,
    requested: &crate::semantic::RedactedText,
    terminal_deadline: Instant,
    call_timeout: Duration,
) -> Result<bool, BackendFailure> {
    let remaining = terminal_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(BackendFailure::new(
            BackendFailureKind::Timeout,
            "exact text verification deadline expired",
        ));
    }
    let (start, end) =
        exact_text_readback_range(position, insertion_start, requested.character_count())?;
    let reply = timed_secret_call(
        remaining.min(call_timeout),
        "Text.GetText",
        text_proxy.inner().call_method("GetText", &(start, end)),
    )
    .await?;
    let body = reply.body();
    let observed = decode_bounded_exact_text_reply(body.signature(), body.data())?;
    exact_text_matches(requested, observed)
}

async fn read_text_evidence(
    text_proxy: &TextProxy<'_>,
    call_timeout: Duration,
) -> Result<TextReadbackEvidence, BackendFailure> {
    let character_count = timed_secret_call(
        call_timeout,
        "Text.CharacterCount",
        text_proxy.character_count(),
    )
    .await?;
    let character_count = u32::try_from(character_count).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text returned a negative character count",
        )
    })?;
    let caret_offset =
        timed_secret_call(call_timeout, "Text.CaretOffset", text_proxy.caret_offset()).await?;
    if caret_offset < -1 || u32::try_from(caret_offset).is_ok_and(|caret| caret > character_count) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text caret offset is below -1 or exceeds character count",
        ));
    }
    let selection_count = timed_secret_call(
        call_timeout,
        "Text.GetNSelections",
        text_proxy.get_n_selections(),
    )
    .await?;
    let selection_count = usize::try_from(selection_count).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text returned a negative selection count",
        )
    })?;
    if selection_count > MAX_SELECTION_RANGES {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text selection readback exceeds the adapter limit",
        ));
    }
    let mut selections = Vec::with_capacity(selection_count);
    for index in 0..selection_count {
        let (start, end) = timed_secret_call(
            call_timeout,
            "Text.GetSelection",
            text_proxy.get_selection(index_to_i32(u32::try_from(index).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "selection index overflow")
            })?)?),
        )
        .await?;
        let start = u32::try_from(start).map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "Text selection start is negative",
            )
        })?;
        let end = u32::try_from(end).map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "Text selection end is negative",
            )
        })?;
        if start > end || end > character_count {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Text selection range is reversed or exceeds character count",
            ));
        }
        selections.push(SelectionRangeEvidence { start, end });
    }
    Ok(TextReadbackEvidence {
        character_count,
        caret_offset,
        selections,
    })
}

async fn apply_text_selection_policy(
    text_proxy: &TextProxy<'_>,
    before: &TextReadbackEvidence,
    after_write: &TextReadbackEvidence,
    insertion_start: u32,
    insertion_end: u32,
    policy: TextSelectionPolicy,
    call_timeout: Duration,
) -> Result<TextReadbackEvidence, BackendFailure> {
    for selection in (0..after_write.selections.len()).rev() {
        let removed = timed_secret_call(
            call_timeout,
            "Text.RemoveSelection",
            text_proxy.remove_selection(index_to_i32(u32::try_from(selection).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "selection index overflow")
            })?)?),
        )
        .await?;
        if !removed {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Text.RemoveSelection declined selection policy",
            ));
        }
    }
    let new_length = after_write.character_count;
    let (caret, selections) = match policy {
        TextSelectionPolicy::Preserve => (
            u32::try_from(before.caret_offset)
                .unwrap_or(0)
                .min(new_length),
            before
                .selections
                .iter()
                .map(|range| SelectionRangeEvidence {
                    start: range.start.min(new_length),
                    end: range.end.min(new_length),
                })
                .collect::<Vec<_>>(),
        ),
        TextSelectionPolicy::CollapseBefore => (insertion_start.min(new_length), Vec::new()),
        TextSelectionPolicy::CollapseAfter => (insertion_end.min(new_length), Vec::new()),
        TextSelectionPolicy::SelectInserted => (
            insertion_end.min(new_length),
            vec![SelectionRangeEvidence {
                start: insertion_start.min(new_length),
                end: insertion_end.min(new_length),
            }],
        ),
    };
    for range in &selections {
        if range.start == range.end {
            continue;
        }
        let added = timed_secret_call(
            call_timeout,
            "Text.AddSelection",
            text_proxy.add_selection(index_to_i32(range.start)?, index_to_i32(range.end)?),
        )
        .await?;
        if !added {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Text.AddSelection declined selection policy",
            ));
        }
    }
    let caret_set = timed_secret_call(
        call_timeout,
        "Text.SetCaretOffset",
        text_proxy.set_caret_offset(index_to_i32(caret)?),
    )
    .await?;
    if !caret_set {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Text.SetCaretOffset declined selection policy",
        ));
    }
    Ok(TextReadbackEvidence {
        character_count: new_length,
        caret_offset: i32::try_from(caret).map_err(|_| {
            BackendFailure::new(BackendFailureKind::Protocol, "caret offset exceeds i32")
        })?,
        selections,
    })
}

async fn component_proxy<'a>(
    connection: &'a zbus::Connection,
    object: &'a ObjectAddress,
    call_timeout: Duration,
) -> Result<ComponentProxy<'a>, BackendFailure> {
    timed_call(
        call_timeout,
        "Component proxy construction",
        ComponentProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(object.bus_name())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .path(object.object_path())
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?
            .build(),
    )
    .await
}

async fn read_extents(
    proxy: &ComponentProxy<'_>,
    call_timeout: Duration,
) -> Result<SemanticRect, BackendFailure> {
    let (x, y, width, height) = timed_call(
        call_timeout,
        "Component.GetExtents",
        proxy.get_extents(CoordType::Screen),
    )
    .await?;
    if width < 0 || height < 0 {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Component.GetExtents returned negative geometry",
        ));
    }
    Ok(SemanticRect {
        x,
        y,
        width,
        height,
    })
}

async fn settle_scroll_extents(
    proxy: &ComponentProxy<'_>,
    before: SemanticRect,
    screen_point: Option<(i32, i32)>,
    accepted: bool,
    terminal_deadline: Instant,
    settle_budget: Duration,
) -> Result<SemanticRect, BackendFailure> {
    let mut settle = SemanticSettle::new(terminal_deadline, settle_budget);
    let mut last = None;
    let mut last_timeout = None;
    while let Some(call_timeout) = settle.next_call_timeout(settle_budget) {
        match timeout(call_timeout, read_extents(proxy, call_timeout)).await {
            Ok(Ok(evidence)) => {
                let reached = scroll_readback_reached(
                    accepted,
                    screen_point,
                    before,
                    last.as_ref(),
                    evidence,
                );
                last = Some(evidence);
                if reached {
                    break;
                }
            }
            Ok(Err(error)) if error.kind == BackendFailureKind::Timeout => {
                last_timeout = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                last_timeout = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "Component extents settling read timed out",
                ));
            }
        }
        if !settle.pause().await {
            break;
        }
    }
    finish_semantic_settle(
        last,
        last_timeout,
        "Component extents settling produced no valid readback",
    )
}

fn scroll_readback_reached(
    accepted: bool,
    screen_point: Option<(i32, i32)>,
    before: SemanticRect,
    previous: Option<&SemanticRect>,
    current: SemanticRect,
) -> bool {
    if !accepted {
        return true;
    }
    if let Some((x, y)) = screen_point {
        return current.x == x && current.y == y;
    }
    current != before && previous == Some(&current)
}

async fn revalidate_live_identity(
    connection: &zbus::Connection,
    identity: &LiveSemanticIdentity,
    require_classified_text: bool,
) -> Result<(), BackendFailure> {
    let application = raw_required_object_ref(
        &bounded_accessible_empty_reply(
            connection,
            &identity.object,
            "GetApplication",
            identity.proxy_call_timeout,
            identity.cache_limits.max_item_bytes,
        )
        .await?,
    )?;
    if application != identity.application {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live semantic application identity changed before dispatch",
        ));
    }
    let parent = raw_optional_object_property(
        connection,
        &identity.object,
        "Parent",
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
    )
    .await?;
    let raw_index = bounded_accessible_empty_reply(
        connection,
        &identity.object,
        "GetIndexInParent",
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
    )
    .await?
    .body()
    .deserialize::<i32>()
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let name = raw_string_property(
        connection,
        &identity.object,
        "Name",
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
        identity.cache_limits.max_string_bytes,
    )
    .await?;
    let description = raw_string_property(
        connection,
        &identity.object,
        "Description",
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
        identity.cache_limits.max_string_bytes,
    )
    .await?;
    let role = read_raw_role(
        connection,
        &identity.object,
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
    )
    .await?;
    let normalized = normalize_live_identity(
        identity.object.clone(),
        application,
        parent,
        raw_index,
        role,
        name,
        description,
        identity.cache_limits,
    )?;
    if normalized.identity_fingerprint() != identity.expected_identity
        || normalized.index_in_parent != identity.expected_index_in_parent
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live semantic identity fingerprint changed before dispatch",
        ));
    }
    if role != identity.expected_role {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live semantic role changed before dispatch",
        ));
    }
    if require_classified_text && !(0..=129).contains(&role) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live unclassified text write denied",
        ));
    }
    let states = read_raw_states(
        connection,
        &identity.object,
        identity.proxy_call_timeout,
        identity.cache_limits.max_item_bytes,
        identity.cache_limits.max_states,
    )
    .await?;
    if states != identity.expected_states {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "live semantic states changed before dispatch",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn normalize_live_identity(
    object: ObjectAddress,
    application: ObjectAddress,
    parent: Option<ObjectAddress>,
    index_in_parent: i32,
    role: u32,
    name: String,
    description: String,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, BackendFailure> {
    normalize_modern(
        ModernCacheItem {
            object,
            application,
            parent,
            index_in_parent,
            child_count: 0,
            interfaces: Vec::new(),
            short_name: name,
            role,
            name: description,
            states: Vec::new(),
        },
        limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn read_raw_role(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<u32, BackendFailure> {
    let reply = bounded_accessible_empty_reply(
        connection,
        object,
        "GetRole",
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    reply
        .body()
        .deserialize::<u32>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn read_raw_interfaces(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    max_reply_bytes: usize,
    max_interfaces: usize,
) -> Result<Vec<String>, BackendFailure> {
    let reply = bounded_accessible_empty_reply(
        connection,
        object,
        "GetInterfaces",
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    let interfaces = reply
        .body()
        .deserialize::<Vec<String>>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if interfaces.len() > max_interfaces {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Accessible.GetInterfaces exceeded the adapter limit",
        ));
    }
    Ok(interfaces)
}

async fn read_raw_states(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    max_reply_bytes: usize,
    max_states: usize,
) -> Result<Vec<u32>, BackendFailure> {
    let reply = bounded_accessible_empty_reply(
        connection,
        object,
        "GetState",
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    let states = reply
        .body()
        .deserialize::<Vec<u32>>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if states.len() > max_states {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Accessible.GetState exceeded the adapter word limit",
        ));
    }
    Ok(states)
}

async fn wait_for_raw_state(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    state_index: usize,
    terminal_deadline: Instant,
    settle_timeout: Duration,
    max_reply_bytes: usize,
    max_states: usize,
) -> Result<bool, BackendFailure> {
    let mut settle = SemanticSettle::new(terminal_deadline, settle_timeout);
    let mut last = None;
    let mut last_timeout = None;
    while let Some(call_timeout) = settle.next_call_timeout(settle_timeout) {
        match timeout(
            call_timeout,
            read_raw_states(
                connection,
                object,
                call_timeout,
                max_reply_bytes,
                max_states,
            ),
        )
        .await
        {
            Ok(Ok(states)) => {
                let reached = raw_state_contains(&states, state_index);
                last = Some(reached);
                if reached {
                    break;
                }
            }
            Ok(Err(error)) if error.kind == BackendFailureKind::Timeout => {
                last_timeout = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                last_timeout = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "Accessible state settling read timed out",
                ));
            }
        }
        if !settle.pause().await {
            break;
        }
    }
    finish_semantic_settle(
        last,
        last_timeout,
        "Accessible state settling produced no valid readback",
    )
}

async fn bounded_accessible_empty_reply(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    member: &'static str,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<zbus::Message, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "bounded Accessible method",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.a11y.atspi.Accessible"),
            member,
            &(),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    Ok(reply)
}

async fn bounded_accessible_index_reply(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    member: &'static str,
    index: i32,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<zbus::Message, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "bounded indexed Accessible method",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.a11y.atspi.Accessible"),
            member,
            &(index,),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    Ok(reply)
}

async fn bounded_accessible_property_reply(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    property: &'static str,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<zbus::Message, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "bounded Accessible property",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.a11y.atspi.Accessible", property),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    Ok(reply)
}

fn validate_reply_bytes(
    reply: &zbus::Message,
    max_reply_bytes: usize,
) -> Result<(), BackendFailure> {
    if reply.body().len() > max_reply_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI reply exceeded the pre-deserialization byte limit",
        ));
    }
    Ok(())
}

fn raw_required_object_ref(reply: &zbus::Message) -> Result<ObjectAddress, BackendFailure> {
    let (bus_name, path) = reply
        .body()
        .deserialize::<RawObjectRef>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    ObjectAddress::new(bus_name, path.to_string())
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn raw_optional_object_property(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    property: &'static str,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<Option<ObjectAddress>, BackendFailure> {
    let reply = bounded_accessible_property_reply(
        connection,
        object,
        property,
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    let body = reply.body();
    let value = body
        .deserialize::<zbus::zvariant::Value<'_>>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let (bus_name, path) = value
        .downcast::<RawObjectRef>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if bus_name.is_empty() || path.as_str() == "/org/a11y/atspi/null" {
        return Ok(None);
    }
    ObjectAddress::new(bus_name, path.to_string())
        .map(Some)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn raw_i32_property(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    property: &'static str,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<i32, BackendFailure> {
    let reply = bounded_accessible_property_reply(
        connection,
        object,
        property,
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    let value = reply
        .body()
        .deserialize::<OwnedValue>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    i32::try_from(value).map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn raw_string_property(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    property: &'static str,
    call_timeout: Duration,
    max_reply_bytes: usize,
    max_string_bytes: usize,
) -> Result<String, BackendFailure> {
    let reply = bounded_accessible_property_reply(
        connection,
        object,
        property,
        call_timeout,
        max_reply_bytes,
    )
    .await?;
    let value = reply
        .body()
        .deserialize::<OwnedValue>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let value = String::try_from(value)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if value.len() > max_string_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Accessible string property exceeded the adapter limit",
        ));
    }
    Ok(value)
}

fn validate_lazy_node_bytes(
    node: &LazyAccessibleNode,
    limits: CacheLimits,
) -> Result<(), BackendFailure> {
    let mut bytes = node.application.bus_name().len()
        + node.application.object_path().len()
        + node.name.len()
        + node.description.len();
    if let Some(parent) = &node.parent {
        bytes = bytes
            .checked_add(parent.bus_name().len() + parent.object_path().len())
            .ok_or_else(|| {
                BackendFailure::new(BackendFailureKind::Protocol, "node byte overflow")
            })?;
    }
    for child in &node.children {
        bytes = bytes
            .checked_add(child.bus_name().len() + child.object_path().len())
            .ok_or_else(|| {
                BackendFailure::new(BackendFailureKind::Protocol, "node byte overflow")
            })?;
    }
    for interface in &node.interfaces {
        bytes = bytes.checked_add(interface.len()).ok_or_else(|| {
            BackendFailure::new(BackendFailureKind::Protocol, "node byte overflow")
        })?;
    }
    bytes = bytes
        .checked_add(node.states.len().saturating_mul(std::mem::size_of::<u32>()))
        .ok_or_else(|| BackendFailure::new(BackendFailureKind::Protocol, "node byte overflow"))?;
    if bytes > limits.max_item_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy Accessible node exceeded the aggregate byte limit",
        ));
    }
    Ok(())
}

async fn timed_call<T, E, F>(
    call_timeout: Duration,
    operation: &'static str,
    future: F,
) -> Result<T, BackendFailure>
where
    F: Future<Output = Result<T, E>>,
{
    timed_secret_call(call_timeout, operation, future).await
}

async fn timed_secret_call<T, E, F>(
    call_timeout: Duration,
    operation: &'static str,
    future: F,
) -> Result<T, BackendFailure>
where
    F: Future<Output = Result<T, E>>,
{
    timeout(call_timeout, future)
        .await
        .map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Timeout,
                format!("{operation} timed out"),
            )
        })?
        .map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                format!("{operation} failed with redacted remote diagnostics"),
            )
        })
}

async fn read_actions(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<Vec<atspi_proxies::common::Action>, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "Action.GetActions",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.a11y.atspi.Action"),
            "GetActions",
            &(),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    let actions = reply
        .body()
        .deserialize::<Vec<atspi_proxies::common::Action>>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    validate_actions(&actions)?;
    if actions.iter().all(|action| !action.name.is_empty()) {
        return Ok(actions);
    }

    // Chromium currently returns the correct GetActions array length while
    // leaving every tuple empty, even though its indexed Action methods expose
    // stable machine-readable names. Fall back only when the bulk reply is
    // demonstrably non-authoritative, and retain all existing count, reply,
    // string, and aggregate byte bounds.
    let count = read_action_count(connection, object, call_timeout, max_reply_bytes).await?;
    if count == 0 && actions.is_empty() {
        return Ok(actions);
    }
    let count = usize::try_from(count).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Action.NActions returned a negative count",
        )
    })?;
    if count > MAX_ACTIONS {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Action.NActions exceeded the adapter action limit",
        ));
    }
    let mut fallback = Vec::with_capacity(count);
    for index in 0..count {
        let index = i32::try_from(index).map_err(|_| {
            BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
        })?;
        fallback.push(atspi_proxies::common::Action {
            name: read_action_string(
                connection,
                object,
                "GetName",
                index,
                call_timeout,
                max_reply_bytes,
            )
            .await?,
            description: read_action_string(
                connection,
                object,
                "GetDescription",
                index,
                call_timeout,
                max_reply_bytes,
            )
            .await?,
            keybinding: read_action_string(
                connection,
                object,
                "GetKeyBinding",
                index,
                call_timeout,
                max_reply_bytes,
            )
            .await?,
        });
    }
    validate_actions(&fallback)?;
    Ok(fallback)
}

async fn read_action_count(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<i32, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "Action.NActions",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.a11y.atspi.Action", "NActions"),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    let value = reply
        .body()
        .deserialize::<OwnedValue>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    i32::try_from(value).map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

async fn read_action_string(
    connection: &zbus::Connection,
    object: &ObjectAddress,
    method: &'static str,
    index: i32,
    call_timeout: Duration,
    max_reply_bytes: usize,
) -> Result<String, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "indexed Action metadata read",
        connection.call_method(
            Some(object.bus_name()),
            object.object_path(),
            Some("org.a11y.atspi.Action"),
            method,
            &(index,),
        ),
    )
    .await?;
    validate_reply_bytes(&reply, max_reply_bytes)?;
    reply
        .body()
        .deserialize::<String>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

fn validate_actions(actions: &[atspi_proxies::common::Action]) -> Result<(), BackendFailure> {
    if actions.len() > MAX_ACTIONS {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Action.GetActions exceeded the adapter action limit",
        ));
    }
    let mut aggregate = 0_usize;
    for action in actions {
        if action.name.chars().any(char::is_control) {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Action.GetActions returned a control character in an action name",
            ));
        }
        for length in [
            action.name.len(),
            action.description.len(),
            action.keybinding.len(),
        ] {
            if length > MAX_ACTION_FIELD_BYTES {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "Action.GetActions exceeded the adapter string limit",
                ));
            }
            aggregate = aggregate.checked_add(length).ok_or_else(|| {
                BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "Action.GetActions aggregate byte accounting overflowed",
                )
            })?;
        }
    }
    if aggregate > MAX_ACTION_EVIDENCE_BYTES {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Action.GetActions exceeded the aggregate evidence byte limit",
        ));
    }
    Ok(())
}

fn resolve_action(
    actions: &[atspi_proxies::common::Action],
    selector: &ActionSelector,
) -> Result<u32, BackendFailure> {
    match selector {
        ActionSelector::Default => {
            let best_rank = actions
                .iter()
                .filter_map(|action| default_action_rank(&action.name))
                .min()
                .ok_or_else(|| {
                    BackendFailure::new(
                        BackendFailureKind::ActionNotFound,
                        "no conventional default action is present on the live target",
                    )
                })?;
            let mut matches = actions
                .iter()
                .enumerate()
                .filter(|(_, action)| default_action_rank(&action.name) == Some(best_rank));
            let (index, _) = matches.next().ok_or_else(|| {
                BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "default action resolution lost its preferred action",
                )
            })?;
            if matches.next().is_some() {
                return Err(BackendFailure::new(
                    BackendFailureKind::AmbiguousAction,
                    "conventional default action is ambiguous",
                ));
            }
            u32::try_from(index).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
            })
        }
        ActionSelector::Index(index) => {
            let index_usize = usize::try_from(*index).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
            })?;
            if index_usize >= actions.len() {
                return Err(BackendFailure::new(
                    BackendFailureKind::ActionNotFound,
                    "action index is not present on the live target",
                ));
            }
            Ok(*index)
        }
        ActionSelector::Name(name) => {
            let mut matches = actions
                .iter()
                .enumerate()
                .filter(|(_, action)| action.name == *name);
            let (index, _) = matches.next().ok_or_else(|| {
                BackendFailure::new(
                    BackendFailureKind::ActionNotFound,
                    "action name is not present on the live target",
                )
            })?;
            if matches.next().is_some() {
                return Err(BackendFailure::new(
                    BackendFailureKind::AmbiguousAction,
                    "action name is ambiguous on the live target",
                ));
            }
            u32::try_from(index).map_err(|_| {
                BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
            })
        }
    }
}

fn default_action_rank(name: &str) -> Option<u8> {
    match name.trim().to_ascii_lowercase().as_str() {
        "click" => Some(0),
        "press" => Some(1),
        "activate" => Some(2),
        "default" => Some(3),
        _ => None,
    }
}

fn action_evidence(
    actions: Vec<atspi_proxies::common::Action>,
) -> Result<Vec<ActionEvidence>, BackendFailure> {
    actions
        .into_iter()
        .enumerate()
        .map(|(index, action)| {
            Ok(ActionEvidence {
                index: u32::try_from(index).map_err(|_| {
                    BackendFailure::new(BackendFailureKind::Protocol, "action index overflow")
                })?,
                name: action.name,
                description: action.description,
                keybinding: action.keybinding,
            })
        })
        .collect()
}

fn index_to_i32(index: u32) -> Result<i32, BackendFailure> {
    i32::try_from(index).map_err(|_| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "semantic index exceeds AT-SPI's signed range",
        )
    })
}

fn raw_state_contains(states: &[u32], state_index: usize) -> bool {
    states
        .get(state_index / u32::BITS as usize)
        .is_some_and(|word| word & (1_u32 << (state_index % u32::BITS as usize)) != 0)
}

fn scroll_type(placement: ScrollPlacement) -> ScrollType {
    match placement {
        ScrollPlacement::TopLeft => ScrollType::TopLeft,
        ScrollPlacement::BottomRight => ScrollType::BottomRight,
        ScrollPlacement::TopEdge => ScrollType::TopEdge,
        ScrollPlacement::BottomEdge => ScrollType::BottomEdge,
        ScrollPlacement::LeftEdge => ScrollType::LeftEdge,
        ScrollPlacement::RightEdge => ScrollType::RightEdge,
        ScrollPlacement::Anywhere => ScrollType::Anywhere,
    }
}

async fn drain_cache_signals<S>(mut stream: S, context: SignalDrainContext)
where
    S: futures_util::Stream<Item = Result<zbus::Message, zbus::Error>> + Unpin,
{
    let SignalDrainContext {
        ingress,
        cache_limits,
        buffer_capacity,
        mut bootstrap_barriers,
        bootstrap_barriers_applied,
        cancellation,
        connection,
    } = context;
    let mut active_barriers = bootstrap_barriers.borrow().clone();
    let mut buffered = VecDeque::new();
    let mut last_receive_position = None;
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            () = connection.closed() => {
                let _result = ingress.offer(BackendEvent::ConnectionClosed);
                break;
            }
            changed = bootstrap_barriers.changed(), if active_barriers.is_none() => {
                if changed.is_err() {
                    break;
                }
                active_barriers = bootstrap_barriers.borrow_and_update().clone();
                if let Some(barriers) = &active_barriers {
                    while let Some(event) = buffered.pop_front() {
                        if offer_after_barrier(&ingress, barriers, event) == EventOfferResult::Closed {
                            return;
                        }
                    }
                    bootstrap_barriers_applied.send_replace(true);
                }
            }
            message = stream.next() => match message {
                Some(Ok(message)) => {
                    if !advance_receive_position(
                        &mut last_receive_position,
                        message.recv_position(),
                    ) {
                        let _result = ingress.offer(BackendEvent::ResyncRequired {
                            reason: "non_monotonic_signal_receive_position",
                        });
                        break;
                    }
                    let header = message.header();
                    let interface = header.interface().map(|value| value.as_str());
                    match interface {
                        Some("org.freedesktop.DBus") => {
                            let member = header.member().map(|value| value.as_str());
                            if member != Some("NameOwnerChanged") {
                                continue;
                            }
                            let event = decode_name_owner_change(&message, cache_limits);
                            let event = match event {
                                Ok(Some(name)) => {
                                    if owner_change_requires_bootstrap_resync(
                                        active_barriers.is_some(),
                                    ) {
                                        let _result = ingress.offer(BackendEvent::ResyncRequired {
                                            reason: "owner_change_during_cache_bootstrap",
                                        });
                                        break;
                                    }
                                    BackendEvent::Cache(CacheEvent::InvalidateApplication(name))
                                }
                                Ok(None) => continue,
                                Err(_) => BackendEvent::ResyncRequired {
                                    reason: "malformed_name_owner_change",
                                },
                            };
                            if ingress.offer(event) == EventOfferResult::Closed {
                                break;
                            }
                        }
                        Some("org.a11y.atspi.Cache") => {
                            if message.body().len() > cache_limits.max_item_bytes {
                                let _result = ingress.offer(BackendEvent::ResyncRequired {
                                    reason: "cache_signal_byte_limit_exceeded",
                                });
                                continue;
                            }
                            let event = decode_cache_signal(&message, cache_limits);
                            let offered = match event {
                                Ok(event) => {
                                    if let Some(barriers) = &active_barriers {
                                        offer_after_barrier(&ingress, barriers, event)
                                    } else if buffered.len() == buffer_capacity {
                                        buffered.clear();
                                        ingress.offer(BackendEvent::ResyncRequired {
                                            reason: "cache_bootstrap_event_buffer_overflow",
                                        })
                                    } else {
                                        buffered.push_back(event);
                                        EventOfferResult::Accepted
                                    }
                                }
                                Err(_) => ingress.offer(BackendEvent::ResyncRequired {
                                    reason: "unsupported_or_malformed_cache_signal",
                                }),
                            };
                            if offered == EventOfferResult::Closed {
                                break;
                            }
                        }
                        Some("org.a11y.atspi.Event.Object")
                            if handle_object_signal_message(
                                &message,
                                &ingress,
                                cache_limits,
                                "org.a11y.atspi.Event.Object",
                            ) =>
                        {
                            break;
                        }
                        Some("org.a11y.atspi.Event.Focus")
                            if handle_object_signal_message(
                                &message,
                                &ingress,
                                cache_limits,
                                "org.a11y.atspi.Event.Focus",
                            ) =>
                        {
                            break;
                        }
                        Some("org.a11y.atspi.Event.Window")
                            if handle_object_signal_message(
                            &message,
                            &ingress,
                            cache_limits,
                            "org.a11y.atspi.Event.Window",
                        ) =>
                        {
                            break;
                        }
                        Some(
                            "org.a11y.atspi.Event.Object"
                            | "org.a11y.atspi.Event.Focus"
                            | "org.a11y.atspi.Event.Window",
                        ) => {}
                        _ => {}
                    }
                }
                Some(Err(error)) => {
                    let _result = ingress.offer(BackendEvent::StreamFailed(backend_error(
                        BackendFailureKind::Stream,
                        error,
                    )));
                    break;
                }
                None => {
                    let _result = ingress.offer(BackendEvent::StreamFailed(BackendFailure::new(
                        BackendFailureKind::Stream,
                        "raw AT-SPI signal stream ended",
                    )));
                    break;
                }
            },
        }
    }
}

fn decode_name_owner_change(
    message: &zbus::Message,
    limits: CacheLimits,
) -> Result<Option<String>, BackendFailure> {
    if message.body().len() > limits.max_item_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "NameOwnerChanged exceeded the bounded body limit",
        ));
    }
    let header = message.header();
    if header.sender().map(|value| value.as_str()) != Some("org.freedesktop.DBus")
        || header.path().map(|value| value.as_str()) != Some("/org/freedesktop/DBus")
        || header.interface().map(|value| value.as_str()) != Some("org.freedesktop.DBus")
        || header.member().map(|value| value.as_str()) != Some("NameOwnerChanged")
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "NameOwnerChanged signal provenance was invalid",
        ));
    }
    let (name, old_owner, new_owner) = message
        .body()
        .deserialize::<(String, String, String)>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if name.starts_with(':') && !old_owner.is_empty() && old_owner != new_owner {
        Ok(Some(name))
    } else {
        Ok(None)
    }
}

fn handle_object_signal_message(
    message: &zbus::Message,
    ingress: &BackendEventIngress,
    limits: CacheLimits,
    expected_interface: &'static str,
) -> bool {
    let event = match decode_object_signal(message, limits, expected_interface) {
        Ok(event) => event,
        Err(_) => {
            let _result = ingress.offer(BackendEvent::ResyncRequired {
                reason: "unsupported_or_malformed_object_signal",
            });
            return true;
        }
    };
    match event.policy {
        ObjectEventPolicy::Forward => {
            ingress.offer(BackendEvent::ObjectChanged {
                source: Some(event.source),
                kind: event.kind.to_owned(),
            }) == EventOfferResult::Closed
        }
        ObjectEventPolicy::Refresh => {
            ingress.offer(BackendEvent::RefreshObject {
                source: event.source,
                kind: event.kind.to_owned(),
            }) == EventOfferResult::Closed
        }
        ObjectEventPolicy::Resync => {
            if ingress.offer(BackendEvent::ObjectChanged {
                source: Some(event.source),
                kind: event.kind.to_owned(),
            }) == EventOfferResult::Closed
            {
                return true;
            }
            let _result = ingress.offer(BackendEvent::ResyncRequired {
                reason: "structural_object_event_requires_rebuild",
            });
            true
        }
    }
}

fn decode_object_signal(
    message: &zbus::Message,
    limits: CacheLimits,
    expected_interface: &'static str,
) -> Result<ObservedObjectSignal, BackendFailure> {
    if message.body().len() > limits.max_item_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI object event exceeded the bounded body limit",
        ));
    }
    let header = message.header();
    let interface = header.interface().map(|value| value.as_str());
    let source = validate_object_signal_source(
        interface,
        expected_interface,
        header.sender().map(|value| value.as_str()),
        header.path().map(|value| value.as_str()),
    )?;
    let member = header.member().map(|value| value.as_str());
    let (kind, policy) = classify_object_event(expected_interface, member)?;
    Ok(ObservedObjectSignal {
        source,
        kind,
        policy,
    })
}

fn validate_object_signal_source(
    actual_interface: Option<&str>,
    expected_interface: &'static str,
    sender: Option<&str>,
    path: Option<&str>,
) -> Result<ObjectAddress, BackendFailure> {
    if actual_interface != Some(expected_interface) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI object event interface provenance mismatch",
        ));
    }
    let sender = sender.ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI object event lacked a unique sender",
        )
    })?;
    let path = path.ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI object event lacked a source path",
        )
    })?;
    let source = ObjectAddress::new(sender, path)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    if source.object_path() == "/org/a11y/atspi/null" {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "AT-SPI object event used the null source path",
        ));
    }
    Ok(source)
}

fn classify_object_event(
    interface: &'static str,
    member: Option<&str>,
) -> Result<(&'static str, ObjectEventPolicy), BackendFailure> {
    let classified = match (interface, member) {
        ("org.a11y.atspi.Event.Focus", Some("Focus")) => {
            ("focus.changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("PropertyChange")) => {
            ("object.property_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("StateChanged")) => {
            ("object.state_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("ChildrenChanged")) => {
            ("object.children_changed", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("ModelChanged")) => {
            ("object.model_changed", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("RowInserted")) => {
            ("object.row_inserted", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("RowReordered")) => {
            ("object.row_reordered", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("RowDeleted")) => {
            ("object.row_deleted", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("ColumnInserted")) => {
            ("object.column_inserted", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("ColumnReordered")) => {
            ("object.column_reordered", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("ColumnDeleted")) => {
            ("object.column_deleted", ObjectEventPolicy::Resync)
        }
        ("org.a11y.atspi.Event.Object", Some("BoundsChanged")) => {
            ("object.bounds_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("LinkSelected")) => {
            ("object.link_selected", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("VisibleDataChanged")) => {
            ("object.visible_data_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("SelectionChanged")) => {
            ("object.selection_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("ActiveDescendantChanged")) => (
            "object.active_descendant_changed",
            ObjectEventPolicy::Refresh,
        ),
        ("org.a11y.atspi.Event.Object", Some("Announcement")) => {
            ("object.announcement", ObjectEventPolicy::Forward)
        }
        ("org.a11y.atspi.Event.Object", Some("AttributesChanged")) => {
            ("object.attributes_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("TextBoundsChanged")) => {
            ("object.text_bounds_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("TextSelectionChanged")) => {
            ("object.text_selection_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("TextChanged")) => {
            ("object.text_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("TextAttributesChanged")) => {
            ("object.text_attributes_changed", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Object", Some("TextCaretMoved")) => {
            ("object.text_caret_moved", ObjectEventPolicy::Refresh)
        }
        ("org.a11y.atspi.Event.Window", Some(member)) => {
            let (kind, policy) = match member {
                "PropertyChange" => ("window.property_changed", ObjectEventPolicy::Refresh),
                "Minimize" => ("window.minimized", ObjectEventPolicy::Refresh),
                "Maximize" => ("window.maximized", ObjectEventPolicy::Refresh),
                "Restore" => ("window.restored", ObjectEventPolicy::Refresh),
                "Close" => ("window.closed", ObjectEventPolicy::Resync),
                "Create" => ("window.created", ObjectEventPolicy::Resync),
                "Reparent" => ("window.reparented", ObjectEventPolicy::Resync),
                "DesktopCreate" => ("window.desktop_created", ObjectEventPolicy::Resync),
                "DesktopDestroy" => ("window.desktop_destroyed", ObjectEventPolicy::Resync),
                "Destroy" => ("window.destroyed", ObjectEventPolicy::Resync),
                "Activate" => ("window.activated", ObjectEventPolicy::Refresh),
                "Deactivate" => ("window.deactivated", ObjectEventPolicy::Refresh),
                "Raise" => ("window.raised", ObjectEventPolicy::Refresh),
                "Lower" => ("window.lowered", ObjectEventPolicy::Refresh),
                "Move" => ("window.moved", ObjectEventPolicy::Refresh),
                "Resize" => ("window.resized", ObjectEventPolicy::Refresh),
                "Shade" => ("window.shaded", ObjectEventPolicy::Refresh),
                // `uUshade` is the historical spelling in the AT-SPI D-Bus XML;
                // some producers repair it to the natural `Unshade` spelling.
                "uUshade" | "Unshade" => ("window.unshaded", ObjectEventPolicy::Refresh),
                "Restyle" => ("window.restyled", ObjectEventPolicy::Refresh),
                _ => {
                    return Err(BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "unsupported AT-SPI window event member",
                    ));
                }
            };
            (kind, policy)
        }
        _ => {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "unsupported AT-SPI object event member",
            ));
        }
    };
    Ok(classified)
}

fn advance_receive_position<T: Copy + Ord>(last: &mut Option<T>, current: T) -> bool {
    if last.is_some_and(|previous| current <= previous) {
        return false;
    }
    *last = Some(current);
    true
}

fn owner_change_requires_bootstrap_resync(barriers_published: bool) -> bool {
    !barriers_published
}

fn offer_after_barrier(
    ingress: &BackendEventIngress,
    barriers: &BootstrapBarriers,
    event: SequencedCacheEvent,
) -> EventOfferResult {
    match barrier_decision(barriers, &event) {
        BarrierDecision::Offer => ingress.offer(event.event),
        BarrierDecision::Suppress => EventOfferResult::Accepted,
        BarrierDecision::Resync => ingress.offer(BackendEvent::ResyncRequired {
            reason: "cache_event_during_lazy_bootstrap",
        }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BarrierDecision {
    Offer,
    Suppress,
    Resync,
}

fn barrier_decision(barriers: &BootstrapBarriers, event: &SequencedCacheEvent) -> BarrierDecision {
    match barriers.get(&event.owner) {
        Some(ApplicationBootstrapBarrier::Degraded) => BarrierDecision::Suppress,
        Some(ApplicationBootstrapBarrier::Ready(position)) => {
            if event.position > *position {
                BarrierDecision::Offer
            } else {
                BarrierDecision::Suppress
            }
        }
        Some(ApplicationBootstrapBarrier::LazyReady(position)) => {
            if event.position > *position {
                BarrierDecision::Offer
            } else {
                BarrierDecision::Resync
            }
        }
        None => BarrierDecision::Offer,
    }
}

async fn fetch_application_cache(
    connection: &zbus::Connection,
    bus_name: &str,
    limits: CacheLimits,
) -> Result<(Vec<NormalizedCacheItem>, Sequence), BackendFailure> {
    let reply = connection
        .call_method(
            Some(bus_name),
            "/org/a11y/atspi/cache",
            Some("org.a11y.atspi.Cache"),
            "GetItems",
            &(),
        )
        .await
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let position = reply.recv_position();
    let body = reply.body();
    if body.len() > limits.max_bootstrap_bytes {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            format!(
                "Cache.GetItems reply byte limit exceeded: {} > {}",
                body.len(),
                limits.max_bootstrap_bytes
            ),
        ));
    }
    let signature = body.signature().to_string();
    let items = match signature.as_str() {
        MODERN_CACHE_SIGNATURE => {
            let raw_items = body
                .deserialize::<Vec<RawModernCacheItem>>()
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
            let items = raw_items
                .iter()
                .cloned()
                .map(|item| convert_modern(item, limits))
                .collect::<Result<Vec<_>, _>>()?;
            validate_application_cache_root(&items, bus_name)?;
            validate_queried_owner(&items, bus_name)?;
            validate_modern_cache_completeness(&raw_items, limits)?;
            items
        }
        LEGACY_CACHE_SIGNATURE => {
            let raw_items = body
                .deserialize::<Vec<RawLegacyCacheItem>>()
                .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
            let items = raw_items
                .iter()
                .cloned()
                .map(|item| convert_legacy(item, limits))
                .collect::<Result<Vec<_>, _>>()?;
            validate_application_cache_root(&items, bus_name)?;
            validate_queried_owner(&items, bus_name)?;
            validate_legacy_cache_completeness(&raw_items, limits)?;
            items
        }
        _ => {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                format!("unsupported Cache.GetItems signature {signature}"),
            ));
        }
    };
    Ok((items, position))
}

async fn fetch_application_cache_with_retry(
    connection: &zbus::Connection,
    bus_name: &str,
    limits: CacheLimits,
    call_timeout: Duration,
) -> Result<(Vec<NormalizedCacheItem>, Sequence), BackendFailure> {
    let mut last_error = None;
    for _attempt in 0..2 {
        match timeout(
            call_timeout,
            fetch_application_cache(connection, bus_name, limits),
        )
        .await
        {
            Ok(Ok(result)) => return Ok(result),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(BackendFailure::new(
                    BackendFailureKind::Timeout,
                    "application Cache.GetItems retry timed out",
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "application Cache.GetItems retry exhausted",
        )
    }))
}

async fn lazy_traversal_marker(
    connection: &zbus::Connection,
    application_root: &ObjectAddress,
    call_timeout: Duration,
) -> Result<Sequence, BackendFailure> {
    let reply = timed_call(
        call_timeout,
        "lazy traversal consistency marker",
        connection.call_method(
            Some(application_root.bus_name()),
            application_root.object_path(),
            Some("org.a11y.atspi.Accessible"),
            "GetApplication",
            &(),
        ),
    )
    .await?;
    let application = reply
        .body()
        .deserialize::<RawObjectRef>()
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
        .and_then(required_raw_address)?;
    if application != *application_root {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "lazy traversal marker application identity changed",
        ));
    }
    Ok(reply.recv_position())
}

fn degraded_application_root(
    bus_name: &str,
    object_path: &str,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, BackendFailure> {
    let object = ObjectAddress::new(bus_name, object_path)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    let mut fallback = normalize_modern(
        ModernCacheItem {
            object: object.clone(),
            application: object,
            parent: None,
            index_in_parent: -1,
            child_count: 0,
            interfaces: vec![
                "org.a11y.atspi.Accessible".to_owned(),
                "org.a11y.atspi.Application".to_owned(),
            ],
            short_name: String::new(),
            role: 75,
            name: String::new(),
            states: Vec::new(),
        },
        limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))?;
    // The placeholder proves only application identity. Reporting zero would
    // falsely claim that a failed traversal established an empty subtree.
    fallback.child_count = None;
    Ok(fallback)
}

fn validate_queried_owner(
    items: &[NormalizedCacheItem],
    bus_name: &str,
) -> Result<(), BackendFailure> {
    if items
        .iter()
        .any(|item| item.object.bus_name() != bus_name || item.application.bus_name() != bus_name)
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache.GetItems returned an object owned by another unique bus name",
        ));
    }
    Ok(())
}

fn validate_application_cache_root(
    items: &[NormalizedCacheItem],
    bus_name: &str,
) -> Result<(), BackendFailure> {
    let mut root_index = None;
    for (index, item) in items.iter().enumerate() {
        if item.object == item.application && root_index.replace(index).is_some() {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Cache.GetItems returned multiple application roots",
            ));
        }
    }
    let root_index = root_index.ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache.GetItems omitted its application root",
        )
    })?;
    let application = items[root_index].object.clone();
    if application.bus_name() != bus_name
        || items.iter().any(|item| item.application != application)
    {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache.GetItems returned mixed application root identity",
        ));
    }

    Ok(())
}

fn raw_address_key(reference: &RawObjectRef) -> (&str, &str) {
    (reference.0.as_str(), reference.1.as_str())
}

fn validate_modern_cache_completeness(
    items: &[RawModernCacheItem],
    limits: CacheLimits,
) -> Result<(), BackendFailure> {
    let objects = items
        .iter()
        .map(|item| raw_address_key(&item.0))
        .collect::<BTreeSet<_>>();
    if objects.len() != items.len() {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache.GetItems returned duplicate objects",
        ));
    }

    let mut children = BTreeMap::<(&str, &str), usize>::new();
    for item in items {
        let object = raw_address_key(&item.0);
        let application = raw_address_key(&item.1);
        let parent = raw_address_key(&item.2);
        if object == application || parent.0.is_empty() {
            continue;
        }
        // GTK may retain transient entries whose parent was concurrently
        // removed from an otherwise usable cache snapshot. Such entries do
        // not prove that a present parent's declared subtree is incomplete.
        if parent.0 != application.0 || !objects.contains(&parent) {
            continue;
        }
        *children.entry(parent).or_default() += 1;
    }

    for item in items {
        // The standard -1 sentinel is intentionally non-authoritative. GTK
        // uses it for live menus, transient cells, and virtual collections;
        // requiring whole-application lazy traversal for those otherwise
        // coherent snapshots causes permanent availability loss.
        let Ok(expected) = usize::try_from(item.4) else {
            continue;
        };
        if expected > limits.max_children || expected > limits.max_nodes {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Cache.GetItems child count exceeded the bounded topology limit",
            ));
        }
        let actual = children
            .get(&raw_address_key(&item.0))
            .copied()
            .unwrap_or(0);
        if actual != expected {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Cache.GetItems omitted one or more declared children",
            ));
        }
    }
    Ok(())
}

fn validate_legacy_cache_completeness(
    items: &[RawLegacyCacheItem],
    limits: CacheLimits,
) -> Result<(), BackendFailure> {
    let objects = items
        .iter()
        .map(|item| raw_address_key(&item.0))
        .collect::<BTreeSet<_>>();
    if objects.len() != items.len() {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache.GetItems returned duplicate objects",
        ));
    }

    for item in items {
        if item.3.len() > limits.max_children || item.3.len() > limits.max_nodes {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Cache.GetItems child list exceeded the bounded topology limit",
            ));
        }
        let application = raw_address_key(&item.1);
        let declared = item.3.iter().map(raw_address_key).collect::<BTreeSet<_>>();
        if declared.len() != item.3.len() {
            return Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "Cache.GetItems returned duplicate legacy child references",
            ));
        }
        for child in declared {
            if child.0 != application.0 || !objects.contains(&child) {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "Cache.GetItems omitted a declared legacy child",
                ));
            }
        }
    }
    Ok(())
}

fn decode_cache_signal(
    message: &zbus::Message,
    limits: CacheLimits,
) -> Result<SequencedCacheEvent, BackendFailure> {
    let header = message.header();
    let member = header.member().map(|member| member.as_str());
    let sender = header.sender().map(|value| value.as_str());
    let body = message.body();
    let signature = body.signature().to_string();
    match (member, signature.as_str()) {
        (Some("AddAccessible"), MODERN_CACHE_ITEM_SIGNATURE) => body
            .deserialize::<RawModernCacheItem>()
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
            .and_then(|item| convert_modern(item, limits))
            .and_then(|item| validate_add_signal_provenance(sender, item))
            .map(|item| {
                let owner = item.application.bus_name().to_owned();
                SequencedCacheEvent {
                    event: BackendEvent::Cache(CacheEvent::Upsert(Box::new(item))),
                    owner,
                    position: message.recv_position(),
                }
            }),
        (Some("AddAccessible"), LEGACY_CACHE_ITEM_SIGNATURE) => body
            .deserialize::<RawLegacyCacheItem>()
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
            .and_then(|item| convert_legacy(item, limits))
            .and_then(|item| validate_add_signal_provenance(sender, item))
            .map(|item| {
                let owner = item.application.bus_name().to_owned();
                SequencedCacheEvent {
                    event: BackendEvent::Cache(CacheEvent::Upsert(Box::new(item))),
                    owner,
                    position: message.recv_position(),
                }
            }),
        (Some("RemoveAccessible"), REMOVE_CACHE_ITEM_SIGNATURE) => body
            .deserialize::<RawObjectRef>()
            .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
            .and_then(required_raw_address)
            .and_then(|address| validate_remove_signal_provenance(sender, address))
            .map(|address| SequencedCacheEvent {
                owner: address.bus_name().to_owned(),
                event: BackendEvent::Cache(CacheEvent::Remove(address)),
                position: message.recv_position(),
            }),
        _ => Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            format!("unsupported Cache signal member/signature {member:?}/{signature}"),
        )),
    }
}

fn validate_add_signal_provenance(
    sender: Option<&str>,
    item: NormalizedCacheItem,
) -> Result<NormalizedCacheItem, BackendFailure> {
    let owner = item.object.bus_name();
    if sender != Some(owner) || item.application.bus_name() != owner {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache AddAccessible sender/object/application provenance mismatch",
        ));
    }
    Ok(item)
}

fn validate_remove_signal_provenance(
    sender: Option<&str>,
    address: ObjectAddress,
) -> Result<ObjectAddress, BackendFailure> {
    if sender != Some(address.bus_name()) {
        return Err(BackendFailure::new(
            BackendFailureKind::Protocol,
            "Cache RemoveAccessible sender/object provenance mismatch",
        ));
    }
    Ok(address)
}

fn convert_modern(
    item: RawModernCacheItem,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, BackendFailure> {
    let (
        object,
        application,
        parent,
        index_in_parent,
        child_count,
        interfaces,
        short_name,
        role,
        name,
        states,
    ) = item;
    normalize_modern(
        ModernCacheItem {
            object: required_raw_address(object)?,
            application: required_raw_address(application)?,
            parent: optional_raw_address(parent)?,
            index_in_parent,
            child_count,
            interfaces,
            short_name,
            role,
            name,
            states,
        },
        limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

fn convert_legacy(
    item: RawLegacyCacheItem,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, BackendFailure> {
    let (object, application, parent, children, interfaces, short_name, role, name, states) = item;
    normalize_legacy(
        LegacyCacheItem {
            object: required_raw_address(object)?,
            application: required_raw_address(application)?,
            parent: optional_raw_address(parent)?,
            children: children
                .iter()
                .cloned()
                .map(required_raw_address)
                .collect::<Result<Vec<_>, _>>()?,
            interfaces,
            short_name,
            role,
            name,
            states,
        },
        limits,
    )
    .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

fn required_raw_address(reference: RawObjectRef) -> Result<ObjectAddress, BackendFailure> {
    optional_raw_address(reference)?.ok_or_else(|| {
        BackendFailure::new(
            BackendFailureKind::Protocol,
            "required Cache object reference was null",
        )
    })
}

fn optional_raw_address(
    (name, path): RawObjectRef,
) -> Result<Option<ObjectAddress>, BackendFailure> {
    if name.is_empty() {
        return Ok(None);
    }
    ObjectAddress::new(name, path.to_string())
        .map(Some)
        .map_err(|error| backend_error(BackendFailureKind::Protocol, error))
}

fn backend_error(kind: BackendFailureKind, error: impl std::fmt::Display) -> BackendFailure {
    BackendFailure::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        future::pending,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;

    #[derive(Debug)]
    struct IdleAwareOrderedStream {
        item: Option<(u64, &'static str)>,
    }

    impl ordered_stream::OrderedStream for IdleAwareOrderedStream {
        type Data = &'static str;
        type Ordering = u64;

        fn poll_next_before(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            before: Option<&Self::Ordering>,
        ) -> Poll<ordered_stream::PollResult<Self::Ordering, Self::Data>> {
            if self
                .item
                .is_some_and(|(ordering, _)| before.is_none_or(|limit| ordering <= *limit))
            {
                let Some((ordering, data)) = self.item.take() else {
                    return Poll::Pending;
                };
                return Poll::Ready(ordered_stream::PollResult::Item { ordering, data });
            }
            if before.is_some() {
                Poll::Ready(ordered_stream::PollResult::NoneBefore)
            } else {
                Poll::Pending
            }
        }
    }

    fn raw_ref(bus: &str, path: &str) -> Result<RawObjectRef, Box<dyn Error>> {
        Ok((bus.to_owned(), OwnedObjectPath::try_from(path.to_owned())?))
    }

    fn text_method_return(text: &str) -> Result<zbus::Message, Box<dyn Error>> {
        let call = zbus::Message::method_call("/test/text", "GetText")?
            .interface("org.a11y.atspi.Text")?
            .build(&(0_i32, 1_i32))?;
        Ok(zbus::Message::method_return(&call.header())?.build(&text)?)
    }

    #[derive(Debug)]
    struct FakeLazySource {
        nodes: BTreeMap<ObjectAddress, LazyAccessibleNode>,
        stall: Option<ObjectAddress>,
    }

    impl LazyAccessibleSource for FakeLazySource {
        fn read_node<'a>(
            &'a mut self,
            object: &'a ObjectAddress,
            _call_timeout: Duration,
            _limits: CacheLimits,
        ) -> BackendFuture<'a, Result<LazyAccessibleNode, BackendFailure>> {
            Box::pin(async move {
                if self.stall.as_ref() == Some(object) {
                    pending::<()>().await;
                }
                self.nodes.get(object).cloned().ok_or_else(|| {
                    BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "fake lazy node was not configured",
                    )
                })
            })
        }
    }

    #[derive(Debug)]
    struct ChangingLazySource {
        root: ObjectAddress,
        reads: usize,
    }

    impl LazyAccessibleSource for ChangingLazySource {
        fn read_node<'a>(
            &'a mut self,
            object: &'a ObjectAddress,
            _call_timeout: Duration,
            _limits: CacheLimits,
        ) -> BackendFuture<'a, Result<LazyAccessibleNode, BackendFailure>> {
            Box::pin(async move {
                if object != &self.root {
                    return Err(BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "changing fake received an unexpected object",
                    ));
                }
                self.reads = self.reads.saturating_add(1);
                Ok(lazy_node(
                    &self.root,
                    None,
                    -1,
                    Vec::new(),
                    if self.reads == 1 { "before" } else { "after" },
                ))
            })
        }
    }

    fn address(bus: &str, suffix: &str) -> Result<ObjectAddress, Box<dyn Error>> {
        Ok(ObjectAddress::new(bus, format!("/test/{suffix}"))?)
    }

    fn lazy_node(
        application: &ObjectAddress,
        parent: Option<ObjectAddress>,
        index_in_parent: i32,
        children: Vec<ObjectAddress>,
        name: impl Into<String>,
    ) -> LazyAccessibleNode {
        LazyAccessibleNode {
            application: application.clone(),
            parent,
            index_in_parent,
            child_count: i32::try_from(children.len()).unwrap_or(i32::MAX),
            children,
            interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
            name: name.into(),
            description: String::new(),
            role: 0,
            states: Vec::new(),
        }
    }

    #[test]
    fn raw_cache_signatures_remain_pinned() {
        assert_eq!(MODERN_CACHE_SIGNATURE, "a((so)(so)(so)iiassusau)");
        assert_eq!(LEGACY_CACHE_SIGNATURE, "a((so)(so)(so)a(so)assusau)");
        assert_eq!(
            <Vec<RawModernCacheItem> as zbus::zvariant::Type>::SIGNATURE.to_string(),
            MODERN_CACHE_SIGNATURE
        );
        assert_eq!(
            <Vec<RawLegacyCacheItem> as zbus::zvariant::Type>::SIGNATURE.to_string(),
            LEGACY_CACHE_SIGNATURE
        );
    }

    #[test]
    fn raw_signal_capacity_partition_is_nonzero_exact_and_balanced() -> Result<(), Box<dyn Error>> {
        assert_eq!(partition_raw_signal_capacity(4), None);
        assert_eq!(
            partition_raw_signal_capacity(LiveAtspiConnector::MAX_RAW_SIGNAL_QUEUE_CAPACITY + 1),
            None
        );
        assert_eq!(partition_raw_signal_capacity(5), Some([1, 1, 1, 1, 1]));

        let partition = partition_raw_signal_capacity(512)
            .ok_or_else(|| std::io::Error::other("valid aggregate capacity was rejected"))?;
        assert_eq!(partition, [103, 103, 102, 102, 102]);
        assert_eq!(partition.iter().sum::<usize>(), 512);
        assert!(partition.iter().all(|capacity| *capacity > 0));
        assert_eq!(partition.iter().max(), Some(&103));
        assert_eq!(partition.iter().min(), Some(&102));
        Ok(())
    }

    #[test]
    fn connector_reports_transport_and_decoded_bounds_separately() {
        let connector = LiveAtspiConnector::default();
        assert_eq!(connector.raw_signal_queue_capacity(), 5);
        assert_eq!(connector.decoded_event_capacity(), 128);
        assert_eq!(
            connector.raw_signal_queue_worst_case_bytes(),
            Some(5 * MAX_RAW_ATSPI_MESSAGE_BYTES)
        );
    }

    #[test]
    fn raw_conversion_preserves_unknown_role_interface_and_state_words()
    -> Result<(), Box<dyn Error>> {
        let item = (
            raw_ref(":1.77", "/test/node")?,
            raw_ref(":1.77", "/test/app")?,
            raw_ref("", "/org/a11y/atspi/null")?,
            -1,
            0,
            vec!["org.example.FutureAccessibilityInterface".to_owned()],
            "future node".to_owned(),
            u32::MAX,
            "future description".to_owned(),
            vec![u32::MAX, 0x8000_0000],
        );
        let normalized = convert_modern(item, CacheLimits::default())?;
        assert_eq!(normalized.role, u32::MAX);
        assert_eq!(
            normalized.interfaces,
            vec!["org.example.FutureAccessibilityInterface"]
        );
        assert_eq!(normalized.states, vec![u32::MAX, 0x8000_0000]);
        assert!(validate_queried_owner(std::slice::from_ref(&normalized), ":1.77").is_ok());
        assert!(validate_queried_owner(&[normalized], ":1.88").is_err());
        Ok(())
    }

    #[test]
    fn application_cache_root_detaches_only_the_external_registry_edge()
    -> Result<(), Box<dyn Error>> {
        let application = address(":1.78", "app")?;
        let child = address(":1.78", "child")?;
        let registry = address(":1.1", "registry")?;
        let items = vec![
            normalize_modern(
                ModernCacheItem {
                    object: child.clone(),
                    application: application.clone(),
                    parent: Some(application.clone()),
                    index_in_parent: 0,
                    child_count: 0,
                    interfaces: Vec::new(),
                    short_name: "child".to_owned(),
                    role: 0,
                    name: String::new(),
                    states: Vec::new(),
                },
                CacheLimits::default(),
            )?,
            normalize_modern(
                ModernCacheItem {
                    object: application.clone(),
                    application: application.clone(),
                    parent: Some(registry),
                    index_in_parent: 4,
                    child_count: 1,
                    interfaces: Vec::new(),
                    short_name: "application".to_owned(),
                    role: 75,
                    name: String::new(),
                    states: Vec::new(),
                },
                CacheLimits::default(),
            )?,
        ];

        validate_application_cache_root(&items, ":1.78")?;

        assert_eq!(items[0].parent.as_ref(), Some(&application));
        assert_eq!(items[0].index_in_parent, Some(0));
        assert_eq!(items[1].parent, None);
        assert_eq!(items[1].index_in_parent, None);
        Ok(())
    }

    #[test]
    fn live_identity_revalidation_uses_transient_menu_canonicalization()
    -> Result<(), Box<dyn Error>> {
        let application = address(":1.78", "app")?;
        let object = address(":1.78", "menu-item")?;
        let parent = address(":1.78", "transient-menu")?;
        let cached = normalize_live_identity(
            object.clone(),
            application.clone(),
            Some(parent.clone()),
            -1,
            35,
            "Nested Menu Action".to_owned(),
            String::new(),
            CacheLimits::default(),
        )?;
        assert_eq!(cached.parent, None);
        assert_eq!(cached.index_in_parent, None);

        let unchanged = normalize_live_identity(
            object.clone(),
            application.clone(),
            Some(parent.clone()),
            -1,
            35,
            "Nested Menu Action".to_owned(),
            String::new(),
            CacheLimits::default(),
        )?;
        assert_eq!(
            unchanged.identity_fingerprint(),
            cached.identity_fingerprint()
        );

        let index_drift = normalize_live_identity(
            object,
            application,
            Some(parent),
            0,
            35,
            "Nested Menu Action".to_owned(),
            String::new(),
            CacheLimits::default(),
        )?;
        assert_ne!(
            index_drift.identity_fingerprint(),
            cached.identity_fingerprint()
        );
        assert_eq!(index_drift.index_in_parent, Some(0));
        Ok(())
    }

    #[test]
    fn application_cache_root_rejects_missing_multiple_and_mixed_roots()
    -> Result<(), Box<dyn Error>> {
        let application = address(":1.79", "app")?;
        let other_application = address(":1.79", "other-app")?;
        let child = address(":1.79", "child")?;
        let item = |object: ObjectAddress, owning_application: ObjectAddress| {
            normalize_modern(
                ModernCacheItem {
                    object,
                    application: owning_application,
                    parent: None,
                    index_in_parent: -1,
                    child_count: 0,
                    interfaces: Vec::new(),
                    short_name: String::new(),
                    role: 0,
                    name: String::new(),
                    states: Vec::new(),
                },
                CacheLimits::default(),
            )
        };

        let missing = vec![item(child.clone(), application.clone())?];
        assert!(validate_application_cache_root(&missing, ":1.79").is_err());

        let multiple = vec![
            item(application.clone(), application.clone())?,
            item(other_application.clone(), other_application.clone())?,
        ];
        assert!(validate_application_cache_root(&multiple, ":1.79").is_err());

        let mixed = vec![
            item(application.clone(), application)?,
            item(child, other_application)?,
        ];
        assert!(validate_application_cache_root(&mixed, ":1.79").is_err());
        Ok(())
    }

    #[test]
    fn modern_cache_completeness_rejects_declared_partial_topology() -> Result<(), Box<dyn Error>> {
        let modern = |object: &str,
                      parent: RawObjectRef,
                      index: i32,
                      child_count: i32,
                      role: u32|
         -> Result<RawModernCacheItem, Box<dyn Error>> {
            Ok((
                raw_ref(":1.80", object)?,
                raw_ref(":1.80", "/test/app")?,
                parent,
                index,
                child_count,
                Vec::new(),
                String::new(),
                role,
                String::new(),
                Vec::new(),
            ))
        };
        let root_parent = raw_ref(":1.1", "/org/a11y/atspi/accessible/root")?;
        let root = modern("/test/app", root_parent.clone(), -1, 1, 75)?;
        let frame = modern("/test/frame", raw_ref(":1.80", "/test/app")?, 0, 1, 23)?;
        let document = modern("/test/document", raw_ref(":1.80", "/test/frame")?, 0, 0, 88)?;

        assert!(
            validate_modern_cache_completeness(
                &[document.clone(), root.clone(), frame.clone()],
                CacheLimits::default(),
            )
            .is_ok()
        );
        assert!(
            validate_modern_cache_completeness(
                &[root.clone(), frame.clone()],
                CacheLimits::default(),
            )
            .is_err()
        );

        let unknown = modern("/test/app", root_parent, -1, -1, 75)?;
        assert!(validate_modern_cache_completeness(&[unknown], CacheLimits::default()).is_ok());

        Ok(())
    }

    #[test]
    fn modern_cache_completeness_accepts_known_unindexed_transient_children()
    -> Result<(), Box<dyn Error>> {
        let root = (
            raw_ref(":1.81", "/test/app")?,
            raw_ref(":1.81", "/test/app")?,
            raw_ref(":1.1", "/org/a11y/atspi/accessible/root")?,
            -1,
            2,
            Vec::new(),
            String::new(),
            75,
            String::new(),
            Vec::new(),
        );
        let indexed = (
            raw_ref(":1.81", "/test/indexed")?,
            raw_ref(":1.81", "/test/app")?,
            raw_ref(":1.81", "/test/app")?,
            0,
            0,
            Vec::new(),
            String::new(),
            43,
            String::new(),
            Vec::new(),
        );
        let transient = (
            raw_ref(":1.81", "/test/transient_menu")?,
            raw_ref(":1.81", "/test/app")?,
            raw_ref(":1.81", "/test/app")?,
            -1,
            0,
            Vec::new(),
            String::new(),
            35,
            String::new(),
            Vec::new(),
        );

        assert!(
            validate_modern_cache_completeness(
                &[root, indexed, transient],
                CacheLimits::default(),
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn legacy_cache_completeness_requires_declared_children_to_be_present()
    -> Result<(), Box<dyn Error>> {
        let root = (
            raw_ref(":1.82", "/test/app")?,
            raw_ref(":1.82", "/test/app")?,
            raw_ref(":1.1", "/org/a11y/atspi/accessible/root")?,
            vec![raw_ref(":1.82", "/test/child")?],
            Vec::new(),
            String::new(),
            75,
            String::new(),
            Vec::new(),
        );
        let child = (
            raw_ref(":1.82", "/test/child")?,
            raw_ref(":1.82", "/test/app")?,
            raw_ref(":1.82", "/test/app")?,
            Vec::new(),
            Vec::new(),
            String::new(),
            43,
            String::new(),
            Vec::new(),
        );
        assert!(
            validate_legacy_cache_completeness(
                &[child.clone(), root.clone()],
                CacheLimits::default(),
            )
            .is_ok()
        );
        assert!(validate_legacy_cache_completeness(&[root], CacheLimits::default()).is_err());
        Ok(())
    }

    #[test]
    fn degraded_application_barrier_discards_partial_incremental_cache()
    -> Result<(), Box<dyn Error>> {
        let item = (
            raw_ref(":1.90", "/test/node")?,
            raw_ref(":1.90", "/test/app")?,
            raw_ref("", "/org/a11y/atspi/null")?,
            -1,
            0,
            Vec::new(),
            String::new(),
            0,
            String::new(),
            Vec::new(),
        );
        let item = convert_modern(item, CacheLimits::default())?;
        let event = SequencedCacheEvent {
            owner: ":1.90".to_owned(),
            event: BackendEvent::Cache(CacheEvent::Upsert(Box::new(item))),
            position: Sequence::default(),
        };
        let mut barriers = BootstrapBarriers::new();
        barriers.insert(":1.90".to_owned(), ApplicationBootstrapBarrier::Degraded);
        assert_eq!(
            barrier_decision(&barriers, &event),
            BarrierDecision::Suppress
        );
        Ok(())
    }

    #[test]
    fn lazy_barrier_forces_resync_for_event_received_during_verified_walk()
    -> Result<(), Box<dyn Error>> {
        let item = convert_modern(
            (
                raw_ref(":1.91", "/test/node")?,
                raw_ref(":1.91", "/test/app")?,
                raw_ref("", "/org/a11y/atspi/null")?,
                -1,
                0,
                Vec::new(),
                String::new(),
                0,
                String::new(),
                Vec::new(),
            ),
            CacheLimits::default(),
        )?;
        let event = SequencedCacheEvent {
            owner: ":1.91".to_owned(),
            event: BackendEvent::Cache(CacheEvent::Upsert(Box::new(item))),
            position: Sequence::default(),
        };
        let mut barriers = BootstrapBarriers::new();
        barriers.insert(
            ":1.91".to_owned(),
            ApplicationBootstrapBarrier::LazyReady(Sequence::default()),
        );
        assert_eq!(barrier_decision(&barriers, &event), BarrierDecision::Resync);
        Ok(())
    }

    #[tokio::test]
    async fn lazy_fallback_builds_a_verified_bounded_subtree() -> Result<(), Box<dyn Error>> {
        let root = address(":1.210", "app")?;
        let first = address(":1.210", "first")?;
        let second = address(":1.210", "second")?;
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root.clone(),
            lazy_node(&root, None, -1, vec![first.clone(), second.clone()], "app"),
        );
        nodes.insert(
            first.clone(),
            lazy_node(&root, Some(root.clone()), 0, Vec::new(), "first"),
        );
        nodes.insert(
            second.clone(),
            lazy_node(&root, Some(root.clone()), 1, Vec::new(), "second"),
        );
        let items = traverse_lazy_application(
            &mut FakeLazySource { nodes, stall: None },
            root.clone(),
            CacheLimits::default(),
            Duration::from_millis(50),
        )
        .await?;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].object, root);
        assert_eq!(items[1].object, first);
        assert_eq!(items[2].object, second);
        assert!(items.iter().all(|item| item.application == items[0].object));
        Ok(())
    }

    #[tokio::test]
    async fn lazy_root_detaches_the_external_registry_parent() -> Result<(), Box<dyn Error>> {
        let root = address(":1.215", "app")?;
        let registry = address(":1.1", "registry")?;
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root.clone(),
            lazy_node(&root, Some(registry), 9, Vec::new(), "app"),
        );

        let items = traverse_lazy_application(
            &mut FakeLazySource { nodes, stall: None },
            root.clone(),
            CacheLimits::default(),
            Duration::from_millis(50),
        )
        .await?;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].object, root);
        assert_eq!(items[0].parent, None);
        assert_eq!(items[0].index_in_parent, None);
        Ok(())
    }

    #[tokio::test]
    async fn lazy_fallback_rejects_a_tree_that_changes_between_verification_passes()
    -> Result<(), Box<dyn Error>> {
        let root = address(":1.214", "app")?;
        let result = traverse_lazy_application(
            &mut ChangingLazySource {
                root: root.clone(),
                reads: 0,
            },
            root,
            CacheLimits::default(),
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(
            result,
            Err(BackendFailure {
                kind: BackendFailureKind::Protocol,
                ..
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn lazy_fallback_rejects_cycles_and_duplicate_children() -> Result<(), Box<dyn Error>> {
        let root = address(":1.211", "app")?;
        let child = address(":1.211", "child")?;

        let duplicate_nodes = BTreeMap::from([(
            root.clone(),
            lazy_node(&root, None, -1, vec![child.clone(), child.clone()], "app"),
        )]);
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: duplicate_nodes,
                    stall: None,
                },
                root.clone(),
                CacheLimits::default(),
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );

        let cycle_nodes = BTreeMap::from([
            (
                root.clone(),
                lazy_node(&root, None, -1, vec![child.clone()], "app"),
            ),
            (
                child,
                lazy_node(&root, Some(root.clone()), 0, vec![root.clone()], "child"),
            ),
        ]);
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: cycle_nodes,
                    stall: None,
                },
                root,
                CacheLimits::default(),
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn lazy_fallback_enforces_node_child_depth_and_byte_budgets() -> Result<(), Box<dyn Error>>
    {
        let root = address(":1.212", "app")?;
        let child = address(":1.212", "child")?;
        let pair = BTreeMap::from([
            (
                root.clone(),
                lazy_node(&root, None, -1, vec![child.clone()], "app"),
            ),
            (
                child.clone(),
                lazy_node(&root, Some(root.clone()), 0, Vec::new(), "child"),
            ),
        ]);
        let node_limits = CacheLimits {
            max_nodes: 1,
            ..CacheLimits::default()
        };
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: pair.clone(),
                    stall: None,
                },
                root.clone(),
                node_limits,
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );
        let child_limits = CacheLimits {
            max_children: 1,
            ..CacheLimits::default()
        };
        let too_many_children = BTreeMap::from([(
            root.clone(),
            lazy_node(
                &root,
                None,
                -1,
                vec![child.clone(), address(":1.212", "other")?],
                "app",
            ),
        )]);
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: too_many_children,
                    stall: None,
                },
                root.clone(),
                child_limits,
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );

        let mut deep = BTreeMap::new();
        let paths = (0..=MAX_LAZY_TRAVERSAL_DEPTH + 1)
            .map(|index| address(":1.212", &format!("deep-{index}")))
            .collect::<Result<Vec<_>, _>>()?;
        for (index, object) in paths.iter().enumerate() {
            let children = paths.get(index + 1).cloned().into_iter().collect();
            deep.insert(
                object.clone(),
                lazy_node(
                    &paths[0],
                    index.checked_sub(1).map(|parent| paths[parent].clone()),
                    if index == 0 { -1 } else { 0 },
                    children,
                    format!("node-{index}"),
                ),
            );
        }
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: deep,
                    stall: None,
                },
                paths[0].clone(),
                CacheLimits::default(),
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );

        let byte_nodes = BTreeMap::from([
            (
                root.clone(),
                lazy_node(&root, None, -1, vec![child.clone()], "a".repeat(3_000)),
            ),
            (
                child,
                lazy_node(&root, Some(root.clone()), 0, Vec::new(), "b".repeat(3_000)),
            ),
        ]);
        let byte_limits = CacheLimits {
            max_nodes: 10,
            max_string_bytes: 4_000,
            max_item_bytes: 4_096,
            max_total_bytes: 5_000,
            max_bootstrap_bytes: 5_000,
            max_interfaces: 4,
            max_states: 4,
            max_children: 4,
        };
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: byte_nodes,
                    stall: None,
                },
                root,
                byte_limits,
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn lazy_fallback_times_out_and_rejects_cross_owner_children() -> Result<(), Box<dyn Error>>
    {
        let root = address(":1.213", "app")?;
        let cross_owner = address(":1.999", "child")?;
        let provenance_nodes = BTreeMap::from([(
            root.clone(),
            lazy_node(&root, None, -1, vec![cross_owner], "app"),
        )]);
        assert!(
            traverse_lazy_application(
                &mut FakeLazySource {
                    nodes: provenance_nodes,
                    stall: None,
                },
                root.clone(),
                CacheLimits::default(),
                Duration::from_millis(50),
            )
            .await
            .is_err()
        );

        let timeout_result = traverse_lazy_application(
            &mut FakeLazySource {
                nodes: BTreeMap::new(),
                stall: Some(root.clone()),
            },
            root,
            CacheLimits::default(),
            Duration::from_millis(1),
        )
        .await;
        assert!(matches!(
            timeout_result,
            Err(BackendFailure {
                kind: BackendFailureKind::Timeout,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn action_evidence_enforces_aggregate_bytes_and_control_free_names() {
        let oversized = (0..MAX_ACTIONS)
            .map(|_| atspi_proxies::common::Action {
                name: "a".repeat(4_096),
                description: "d".repeat(4_096),
                keybinding: "k".repeat(4_096),
            })
            .collect::<Vec<_>>();
        assert!(validate_actions(&oversized).is_err());
        assert!(
            validate_actions(&[atspi_proxies::common::Action {
                name: "click\nsecond-line".to_owned(),
                description: String::new(),
                keybinding: String::new(),
            }])
            .is_err()
        );
    }

    #[test]
    fn default_action_resolution_is_ranked_unique_and_never_guesses() {
        let action = |name: &str| atspi_proxies::common::Action {
            name: name.to_owned(),
            description: String::new(),
            keybinding: String::new(),
        };
        let ranked = vec![action("activate"), action(" PRESS "), action("Click")];
        assert_eq!(resolve_action(&ranked, &ActionSelector::Default), Ok(2));
        assert!(
            resolve_action(
                &[action("click"), action("CLICK")],
                &ActionSelector::Default,
            )
            .is_err()
        );
        assert!(resolve_action(&[action("klicken")], &ActionSelector::Default).is_err());
        assert!(resolve_action(&[], &ActionSelector::Default).is_err());
    }

    #[test]
    fn object_event_classification_separates_refresh_forward_and_structural_resync() {
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Focus", Some("Focus")),
            Ok(("focus.changed", ObjectEventPolicy::Refresh))
        );
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Object", Some("TextChanged")),
            Ok(("object.text_changed", ObjectEventPolicy::Refresh))
        );
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Object", Some("Announcement")),
            Ok(("object.announcement", ObjectEventPolicy::Forward))
        );
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Object", Some("ChildrenChanged")),
            Ok(("object.children_changed", ObjectEventPolicy::Resync))
        );
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Window", Some("uUshade")),
            Ok(("window.unshaded", ObjectEventPolicy::Refresh))
        );
        assert_eq!(
            classify_object_event("org.a11y.atspi.Event.Window", Some("Unshade")),
            Ok(("window.unshaded", ObjectEventPolicy::Refresh))
        );
        assert!(
            classify_object_event("org.a11y.atspi.Event.Object", Some("FutureSecretEvent"))
                .is_err()
        );
        let protected_payload = "never-deserialize-this-password";
        let classified = classify_object_event("org.a11y.atspi.Event.Object", Some("TextChanged"));
        assert!(!format!("{classified:?}").contains(protected_payload));
    }

    #[test]
    fn object_event_source_requires_exact_interface_unique_sender_and_nonnull_path() {
        let interface = "org.a11y.atspi.Event.Object";
        assert!(
            validate_object_signal_source(
                Some(interface),
                interface,
                Some(":1.220"),
                Some("/test/node"),
            )
            .is_ok()
        );
        assert!(
            validate_object_signal_source(
                Some("org.a11y.atspi.Event.Focus"),
                interface,
                Some(":1.220"),
                Some("/test/node"),
            )
            .is_err()
        );
        assert!(
            validate_object_signal_source(Some(interface), interface, None, Some("/test/node"))
                .is_err()
        );
        assert!(
            validate_object_signal_source(
                Some(interface),
                interface,
                Some("well-known"),
                Some("/test/node")
            )
            .is_err()
        );
        assert!(
            validate_object_signal_source(Some(interface), interface, Some(":1.220"), None)
                .is_err()
        );
        assert!(
            validate_object_signal_source(
                Some(interface),
                interface,
                Some(":1.220"),
                Some("/org/a11y/atspi/null"),
            )
            .is_err()
        );
    }

    #[test]
    fn cache_signal_provenance_rejects_cross_owner_injection() -> Result<(), Box<dyn Error>> {
        let item = (
            raw_ref(":1.201", "/test/node")?,
            raw_ref(":1.201", "/test/app")?,
            raw_ref("", "/org/a11y/atspi/null")?,
            -1,
            0,
            Vec::new(),
            String::new(),
            0,
            String::new(),
            Vec::new(),
        );
        let item = convert_modern(item, CacheLimits::default())?;
        assert!(validate_add_signal_provenance(Some(":1.202"), item.clone()).is_err());
        assert!(validate_add_signal_provenance(Some(":1.201"), item).is_ok());
        let address = ObjectAddress::new(":1.201", "/test/node")?;
        assert!(validate_remove_signal_provenance(Some(":1.202"), address.clone()).is_err());
        assert!(validate_remove_signal_provenance(Some(":1.201"), address).is_ok());
        Ok(())
    }

    #[test]
    fn merged_stream_position_fence_rejects_duplicate_or_reversed_input() {
        let mut last = None;
        assert!(advance_receive_position(&mut last, 20_u64));
        assert!(!advance_receive_position(&mut last, 19_u64));
        assert_eq!(last, Some(20));
        assert!(!advance_receive_position(&mut last, 20_u64));
        assert!(advance_receive_position(&mut last, 21_u64));
    }

    #[tokio::test]
    async fn ordered_join_restores_global_order_across_ready_streams() {
        let ordering: fn(&(u64, &str)) -> u64 = |item| item.0;
        let first = ordered_stream::FromStream::with_ordering(
            futures_util::stream::iter([(2_u64, "two"), (4, "four")]),
            ordering,
        );
        let second = ordered_stream::FromStream::with_ordering(
            futures_util::stream::iter([(1_u64, "one"), (3, "three")]),
            ordering,
        );
        let joined = ordered_stream::JoinMultiple(vec![
            ordered_stream::OrderedStreamExt::peekable(first),
            ordered_stream::OrderedStreamExt::peekable(second),
        ]);
        let mut stream = ordered_stream::OrderedStreamExt::into_stream(joined);
        let mut observed = Vec::new();
        while let Some((sequence, _label)) = stream.next().await {
            observed.push(sequence);
        }
        assert_eq!(observed, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn ordered_join_advances_when_four_signal_classes_are_idle() -> Result<(), Box<dyn Error>>
    {
        let mut streams = (0..4)
            .map(|_| IdleAwareOrderedStream { item: None })
            .map(ordered_stream::OrderedStreamExt::peekable)
            .collect::<Vec<_>>();
        streams.push(ordered_stream::OrderedStreamExt::peekable(
            IdleAwareOrderedStream {
                item: Some((7, "cache")),
            },
        ));
        let joined = ordered_stream::JoinMultiple(streams);
        let mut stream = ordered_stream::OrderedStreamExt::into_stream(joined);

        let observed = timeout(Duration::from_millis(50), stream.next()).await?;
        assert_eq!(observed, Some("cache"));
        Ok(())
    }

    #[test]
    fn buffered_add_then_owner_loss_during_bootstrap_requires_resync() {
        assert!(owner_change_requires_bootstrap_resync(false));
        assert!(!owner_change_requires_bootstrap_resync(true));
    }

    #[test]
    fn bootstrap_aggregate_bytes_fail_before_cross_application_append() -> Result<(), Box<dyn Error>>
    {
        let item = (
            raw_ref(":1.203", "/test/node")?,
            raw_ref(":1.203", "/test/app")?,
            raw_ref("", "/org/a11y/atspi/null")?,
            -1,
            0,
            Vec::new(),
            "bounded".to_owned(),
            0,
            String::new(),
            Vec::new(),
        );
        let item = convert_modern(item, CacheLimits::default())?;
        let limits = CacheLimits::default();
        assert!(account_bootstrap_items(limits.max_total_bytes, &[item], limits).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn remote_errors_are_redacted_for_ordinary_and_secret_calls() {
        for result in [
            timed_call(Duration::from_millis(50), "Accessible.Name", async {
                Err::<(), _>("submitted-super-secret")
            })
            .await,
            timed_secret_call(Duration::from_millis(50), "Text.CharacterCount", async {
                Err::<(), _>("submitted-super-secret")
            })
            .await,
        ] {
            let error = match result {
                Err(error) => error,
                Ok(()) => return,
            };
            assert!(!error.to_string().contains("submitted-super-secret"));
            assert!(!format!("{error:?}").contains("submitted-super-secret"));
            assert!(error.to_string().contains("redacted remote diagnostics"));
        }
    }

    #[test]
    fn unknown_child_count_is_valid_only_for_shallow_exact_refresh() {
        let limits = CacheLimits::default();
        assert_eq!(bounded_child_count(-1, false, limits).ok(), Some(0));
        assert!(bounded_child_count(-1, true, limits).is_err());
        assert!(bounded_child_count(-2, false, limits).is_err());
    }

    #[test]
    fn semantic_settle_honors_absolute_deadline_call_ceiling_and_attempt_bound() {
        let now = Instant::now();
        let mut deadline_bounded =
            SemanticSettle::new(now + Duration::from_millis(50), Duration::from_secs(2));
        let timeout = deadline_bounded
            .next_call_timeout(Duration::from_secs(1))
            .unwrap_or(Duration::ZERO);
        assert!(!timeout.is_zero());
        assert!(timeout <= Duration::from_millis(50));

        let mut call_bounded =
            SemanticSettle::new(now + Duration::from_secs(5), Duration::from_secs(2));
        assert!(
            call_bounded
                .next_call_timeout(Duration::from_millis(7))
                .is_some_and(|timeout| timeout <= Duration::from_millis(7))
        );
        assert_eq!(
            call_bounded.next_pause_duration(),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            call_bounded.next_pause_duration(),
            Some(Duration::from_millis(20))
        );
        assert_eq!(
            call_bounded.next_pause_duration(),
            Some(Duration::from_millis(40))
        );
        assert_eq!(
            call_bounded.next_pause_duration(),
            Some(Duration::from_millis(80))
        );
        assert_eq!(
            call_bounded.next_pause_duration(),
            Some(Duration::from_millis(100))
        );

        let mut attempt_bounded =
            SemanticSettle::new(now + Duration::from_secs(5), Duration::from_secs(2));
        for _ in 0..SEMANTIC_SETTLE_MAX_ATTEMPTS {
            assert!(
                attempt_bounded
                    .next_call_timeout(Duration::from_millis(1))
                    .is_some()
            );
        }
        assert!(
            attempt_bounded
                .next_call_timeout(Duration::from_millis(1))
                .is_none()
        );

        let timeout = BackendFailure::new(BackendFailureKind::Timeout, "later timeout");
        assert_eq!(
            finish_semantic_settle(Some(7_u32), Some(timeout.clone()), "no readback"),
            Ok(7)
        );
        assert_eq!(
            finish_semantic_settle::<u32>(None, Some(timeout.clone()), "no readback"),
            Err(timeout)
        );
    }

    #[test]
    fn semantic_value_convergence_normalizes_signed_zero_only() {
        assert!(semantic_value_reached(0.0, -0.0));
        assert!(semantic_value_reached(42.5, 42.5));
        assert!(!semantic_value_reached(
            42.5,
            f64::from_bits(42.5_f64.to_bits() + 1)
        ));
    }

    #[test]
    fn selection_settling_predicates_cover_every_operation() {
        assert!(selection_readback_reached(
            SelectionOperation::Clear,
            0,
            None,
            None
        ));
        assert!(!selection_readback_reached(
            SelectionOperation::Clear,
            1,
            None,
            None
        ));
        assert!(selection_readback_reached(
            SelectionOperation::SelectChild(2),
            1,
            Some(true),
            None
        ));
        assert!(selection_readback_reached(
            SelectionOperation::DeselectChild(2),
            0,
            Some(false),
            None
        ));
        assert!(selection_readback_reached(
            SelectionOperation::SelectAll,
            4,
            None,
            Some(4)
        ));
        assert!(!selection_readback_reached(
            SelectionOperation::SelectAll,
            4,
            None,
            None
        ));
    }

    #[test]
    fn text_settling_supports_length_then_exact_policy_evidence() {
        let evidence = TextReadbackEvidence {
            character_count: 8,
            caret_offset: 8,
            selections: vec![SelectionRangeEvidence { start: 3, end: 8 }],
        };
        assert!(TextSettleExpectation::CharacterCount(8).reached(&evidence));
        assert!(TextSettleExpectation::Exact(evidence.clone()).reached(&evidence));
        assert!(!TextSettleExpectation::CharacterCount(7).reached(&evidence));
        let mut different = evidence.clone();
        different.caret_offset = 3;
        assert!(!TextSettleExpectation::Exact(different).reached(&evidence));
    }

    #[test]
    fn exact_text_readback_is_unicode_scalar_bounded_and_content_free() -> Result<(), Box<dyn Error>>
    {
        const SECRET: &str = "é🦀a";
        let requested = crate::semantic::RedactedText::new(SECRET)?;
        assert_eq!(requested.character_count(), 3);
        assert_eq!(
            exact_text_readback_range(None, 17, requested.character_count())?,
            (0, 3)
        );
        assert_eq!(
            exact_text_readback_range(
                Some(TextInsertPosition::Offset(2)),
                2,
                requested.character_count(),
            )?,
            (2, 5)
        );
        assert!(exact_text_matches(&requested, SECRET)?);
        assert!(!exact_text_matches(&requested, "éa🦀")?);
        assert!(!format!("{requested:?}").contains(SECRET));

        let oversized = "x".repeat(crate::semantic::MAX_SEMANTIC_TEXT_BYTES + 1);
        let error = exact_text_matches(&requested, &oversized);
        assert!(matches!(
            error,
            Err(BackendFailure {
                kind: BackendFailureKind::Protocol,
                ..
            })
        ));
        assert!(!format!("{error:?}").contains(SECRET));
        Ok(())
    }

    #[test]
    fn exact_text_reply_is_bounded_before_zero_copy_deserialization() -> Result<(), Box<dyn Error>>
    {
        const SECRET: &str = "é🦀a";
        let reply = text_method_return(SECRET)?;
        let body = reply.body();
        assert_eq!(body.len(), SECRET.len() + DBUS_STRING_BODY_OVERHEAD_BYTES);
        let encoded = body.data().bytes();
        let encoded_start = encoded.as_ptr() as usize;
        let encoded_end = encoded_start + encoded.len();
        let observed = decode_bounded_exact_text_reply(body.signature(), body.data())?;
        let observed_start = observed.as_ptr() as usize;
        assert!(observed_start >= encoded_start);
        assert!(observed_start + observed.len() <= encoded_end);
        assert_eq!(observed, SECRET);

        let boundary = "x".repeat(crate::semantic::MAX_SEMANTIC_TEXT_BYTES);
        let boundary_reply = text_method_return(&boundary)?;
        let boundary_body = boundary_reply.body();
        assert_eq!(boundary_body.len(), MAX_EXACT_TEXT_REPLY_BODY_BYTES);
        assert_eq!(
            decode_bounded_exact_text_reply(boundary_body.signature(), boundary_body.data())?.len(),
            crate::semantic::MAX_SEMANTIC_TEXT_BYTES
        );

        let oversized = "x".repeat(crate::semantic::MAX_SEMANTIC_TEXT_BYTES + 1);
        let oversized_reply = text_method_return(&oversized)?;
        let oversized_body = oversized_reply.body();
        assert_eq!(oversized_body.len(), MAX_EXACT_TEXT_REPLY_BODY_BYTES + 1);
        let failure =
            decode_bounded_exact_text_reply(oversized_body.signature(), oversized_body.data());
        assert!(matches!(
            failure,
            Err(BackendFailure {
                kind: BackendFailureKind::Protocol,
                ..
            })
        ));
        assert!(!format!("{failure:?}").contains(SECRET));
        Ok(())
    }

    #[test]
    fn exact_text_reply_rejects_signature_nul_malformed_and_trailing_content()
    -> Result<(), Box<dyn Error>> {
        let reply = text_method_return("bounded")?;
        let body = reply.body();
        let alternate_signature = zbus::zvariant::Signature::I32;
        assert!(
            decode_bounded_exact_text_reply(&alternate_signature, body.data()).is_err(),
            "an alternate body signature must fail before decoding"
        );

        let context = body.data().context();
        let mut trailing_bytes = body.data().bytes().to_vec();
        trailing_bytes.push(0x7f);
        let trailing = zbus::zvariant::serialized::Data::new(trailing_bytes, context);
        assert!(
            decode_bounded_exact_text_reply(body.signature(), &trailing).is_err(),
            "trailing encoded bytes must be rejected"
        );

        let mut malformed_bytes = body.data().bytes().to_vec();
        malformed_bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let malformed = zbus::zvariant::serialized::Data::new(malformed_bytes, context);
        assert!(
            decode_bounded_exact_text_reply(body.signature(), &malformed).is_err(),
            "a malformed D-Bus string length must be rejected"
        );

        let embedded_nul_length = match context.endian() {
            zbus::zvariant::Endian::Little => 3_u32.to_le_bytes(),
            zbus::zvariant::Endian::Big => 3_u32.to_be_bytes(),
        };
        let embedded_nul_bytes = [embedded_nul_length.as_slice(), b"a\0b", &[0_u8]].concat();
        let embedded_nul = zbus::zvariant::serialized::Data::new(embedded_nul_bytes, context);
        assert!(
            decode_bounded_exact_text_reply(body.signature(), &embedded_nul).is_err(),
            "embedded NUL content must remain fail-closed"
        );
        Ok(())
    }

    #[test]
    fn length_only_mode_never_requests_exact_content_readback() {
        assert!(!exact_text_readback_required(
            TextVerificationMode::LengthOnly
        ));
        assert!(exact_text_readback_required(TextVerificationMode::Exact));
    }

    #[test]
    fn scroll_settling_requires_exact_point_or_stable_changed_extents() {
        let before = SemanticRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        let moved = SemanticRect { y: 5, ..before };
        assert!(!scroll_readback_reached(true, None, before, None, before));
        assert!(!scroll_readback_reached(
            true,
            None,
            before,
            Some(&before),
            moved
        ));
        assert!(scroll_readback_reached(
            true,
            None,
            before,
            Some(&moved),
            moved
        ));
        assert!(scroll_readback_reached(
            true,
            Some((10, 5)),
            before,
            None,
            moved
        ));
        assert!(scroll_readback_reached(false, None, before, None, before));
    }

    #[test]
    fn degraded_application_root_is_bounded_and_identity_only() -> Result<(), Box<dyn Error>> {
        let fallback = degraded_application_root(
            ":1.204",
            "/org/a11y/atspi/accessible/root",
            CacheLimits::default(),
        )?;
        assert_eq!(fallback.object, fallback.application);
        assert!(fallback.name.is_empty());
        assert!(fallback.description.is_empty());
        assert_eq!(fallback.role, 75);
        assert_eq!(fallback.child_count, None);
        Ok(())
    }
}
