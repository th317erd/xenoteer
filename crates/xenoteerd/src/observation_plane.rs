//! Daemon-owned normalization, identity, query, wait, and token composition.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fmt::{self, Write as _},
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tokio::sync::{Notify, oneshot};
use xenoteer_core::{
    AccessibilityCorrelationError, AccessibilityWindowCandidate, ElementClickOcclusionSnapshot,
    MAX_ACCESSIBILITY_CORRELATION_CANDIDATES, MAX_ELEMENT_CLICK_OCCLUDERS, MonotonicMillis,
    NormalizedCorrelationText, WindowContinuationDescriptor, WindowContinuationQuery, WindowModel,
    WindowModelChange, WindowModelError, WindowModelLimits, WindowPageProjection, WindowQueryError,
    WindowQueryView, WindowResolveProjection,
};
use xenoteer_processd::{
    BrokerClient, BrokerPidCorrelation, BrokerPidCorrelationEvidence, MAX_PROCESS_CORRELATION_PIDS,
};
use xenoteer_protocol::{
    CoordinateSpace, DesktopGeneration, DesktopId, EventTopic, MAX_WINDOW_ATOMS,
    MAX_WINDOW_PAGE_LIMIT, MAX_WINDOW_WARNINGS, NormalizedEvent, ProcessRef, Rect,
    SCREEN_DAMAGED_TOPIC, ScreenDamageCoverage, ScreenDamageEvent, WINDOW_CHANGED_TOPIC,
    WINDOW_CREATED_TOPIC, WINDOW_DESTROYED_TOPIC, WINDOW_MODEL_REBUILT_TOPIC, WindowAtomName,
    WindowFocusEvent, WindowGeometry, WindowGeometryEvent, WindowIdentityHash,
    WindowLifecycleEvent, WindowLifecycleKind, WindowListPage, WindowListRequest, WindowMetadata,
    WindowMetadataEvent, WindowMetadataField, WindowModelRebuildReason, WindowModelRebuiltEvent,
    WindowModelRevision, WindowObservedState, WindowOrder, WindowPageCursor, WindowPredicate,
    WindowProcessConfidence, WindowProcessCorrelation, WindowProcessEvidence, WindowQueryPage,
    WindowQueryRequest, WindowRect, WindowRef, WindowReferenceToken, WindowResolveRequest,
    WindowResolveResult, WindowSelector, WindowSnapshot, WindowSnapshotEntry,
    WindowSnapshotRequest, WindowSnapshotResult, WindowSnapshotTarget, WindowSnapshotWarning,
    WindowWaitPredicate, WindowWaitRequest, WindowWaitResult, WindowWaitStatus, WindowWaitTarget,
};
use xenoteer_server::{
    ControlPlaneError, ControlRequestContext, Grant, ObservationFuture, ObservationPlane,
};
use xenoteer_x11::{
    FocusAncestryStatus, KnownAtom, MAX_SNAPSHOT_INPUT_WARNINGS, ObservedAtom,
    ObservedPropertyWarning, PropertyWarning, WindowSnapshotInput,
};
use xenoteer_x11::{
    InventorySource, InventoryWarning, MAX_ROOT_WINDOWS, ObservationActorEvent,
    ObservationActorEventReceiver, ObservationActorExit, ObservationActorFailureKind,
    ObservationActorHandle, ObservationActorJoin, ObservationActorSubmitError, ObservationReply,
    ReconcileDecision, RootDamageBatch, RootDamageCoverage, RootDamageRect, RootInventory,
    spawn_observation_actor,
};

const IDENTITY_HASH_DOMAIN: &[u8] = b"xenoteer-window-identity-v1\0";
const CURSOR_TOKEN_DOMAIN: &[u8] = b"xenoteer-window-cursor-token-v1\0";
const REFERENCE_TOKEN_DOMAIN: &[u8] = b"xenoteer-window-reference-token-v1\0";
const SELECTOR_FINGERPRINT_DOMAIN: &[u8] = b"xenoteer-window-selector-v1\0";
const TOKEN_SECRET_BYTES: usize = 32;
const MAX_TOKEN_MINT_ATTEMPTS: usize = 16;
/// Maximum total time advisory process correlation may add to one response.
const PROCESS_CORRELATION_TOTAL_TIMEOUT: Duration = Duration::from_millis(250);
const PROCESS_CORRELATION_REFRESH_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessCorrelationFence {
    revision: WindowModelRevision,
    lifecycle_authority: ProcessLifecycleAuthorityFence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessLifecycleAuthorityFence(u64);

impl ProcessLifecycleAuthorityFence {
    fn available(self) -> bool {
        self.0 % 2 == 1
    }
}

struct ProcessLifecycleAuthority {
    state: AtomicU64,
}

impl ProcessLifecycleAuthority {
    fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    fn capture(&self) -> ProcessLifecycleAuthorityFence {
        ProcessLifecycleAuthorityFence(self.state.load(Ordering::Acquire))
    }

    fn enable(&self) -> ProcessLifecycleAuthorityFence {
        self.transition(false)
    }

    fn disable(&self) -> ProcessLifecycleAuthorityFence {
        self.transition(true)
    }

    fn transition(&self, disable: bool) -> ProcessLifecycleAuthorityFence {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let currently_disabled = current.is_multiple_of(2);
            if currently_disabled == disable {
                return ProcessLifecycleAuthorityFence(current);
            }
            let next = current.wrapping_add(1);
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return ProcessLifecycleAuthorityFence(next),
                Err(observed) => current = observed,
            }
        }
    }

    fn accepts(&self, fence: ProcessLifecycleAuthorityFence) -> bool {
        fence.available() && self.capture() == fence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessCorrelationCommit {
    fence: ProcessCorrelationFence,
    change_sequence: u64,
}

/// One actor-owned, revision-fenced view used by accessibility correlation.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
pub(crate) struct ObservationCorrelationSnapshot {
    /// Window-model revision shared by every projected snapshot.
    pub(crate) revision: WindowModelRevision,
    /// Actor-owned process-correlation authority epoch.
    pub(crate) correlation_epoch: u64,
    /// Monotonic time at which the model owner produced the view.
    pub(crate) observed_at: MonotonicMillis,
    /// Complete live set, rejected rather than truncated above the hard cap.
    pub(crate) windows: Vec<WindowSnapshot>,
}

#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
impl ObservationCorrelationSnapshot {
    /// Converts the immutable model view into the core correlation boundary.
    ///
    /// X11 titles, class values, and client PIDs remain advisory. Independently
    /// managed process identity, exact root geometry, and exact related window
    /// references retain their stronger provenance for the pure policy.
    pub(crate) fn candidates(
        &self,
    ) -> Result<Vec<AccessibilityWindowCandidate>, AccessibilityCorrelationError> {
        self.windows
            .iter()
            .map(|snapshot| {
                let title = snapshot
                    .metadata
                    .visible_title
                    .as_ref()
                    .or(snapshot.metadata.title.as_ref())
                    .map(|value| NormalizedCorrelationText::new(&value.value))
                    .transpose()?;
                let application_identity = snapshot
                    .metadata
                    .class
                    .as_ref()
                    .and_then(|class| class.class.as_ref())
                    .map(|value| NormalizedCorrelationText::new(&value.value))
                    .transpose()?;
                let toolkit_identity = snapshot
                    .metadata
                    .class
                    .as_ref()
                    .and_then(|class| class.instance.as_ref())
                    .map(|value| NormalizedCorrelationText::new(&value.value))
                    .transpose()?;
                Ok(AccessibilityWindowCandidate {
                    window: snapshot.window.clone(),
                    live: true,
                    process_id: snapshot.process.reported_pid,
                    managed_process_id: snapshot.process.managed_process.map(|process| process.pid),
                    top_level_extents: snapshot
                        .geometry
                        .as_ref()
                        .map(|geometry| geometry.client_rect.rect),
                    title,
                    application_identity,
                    toolkit_identity,
                    focused: snapshot.state.focused,
                    // The current window model does not yet retain transition or
                    // birth instants. Absence is safer than manufacturing times.
                    focus_changed_at: None,
                    created_at: None,
                    observed_at: self.observed_at,
                    client_leader: snapshot.client_leader.clone(),
                })
            })
            .collect()
    }
}

/// Bounded queue-head stacking evidence for one exact live window birth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowOcclusionSnapshot {
    /// Exact target birth resolved by the model owner.
    pub(crate) target_window: WindowRef,
    /// Window-model revision used for target and higher-window geometry.
    pub(crate) model_revision: WindowModelRevision,
    /// Current target client bounds, allowing movement to fail closed.
    pub(crate) target_client_bounds: Option<Rect>,
    /// Nonzero actor-local epoch advanced for each successful stacking read.
    pub(crate) stacking_epoch: u64,
    /// Root-physical rectangles in bottom-to-top order above the target.
    pub(crate) rectangles_above: Vec<Rect>,
    /// False when authoritative stacking or bounded geometry was unavailable.
    pub(crate) stacking_complete: bool,
}

#[allow(dead_code, reason = "consumed by the deferred input precondition")]
impl WindowOcclusionSnapshot {
    /// Borrows this owned actor result as the pure click-policy input.
    pub(crate) fn as_click_snapshot(&self) -> ElementClickOcclusionSnapshot<'_> {
        ElementClickOcclusionSnapshot {
            target_window: &self.target_window,
            stacking_epoch: self.stacking_epoch,
            rectangles_above: &self.rectangles_above,
            stacking_complete: self.stacking_complete,
        }
    }
}

/// Nonblocking normalized-window-event publication failure.
#[allow(dead_code)] // Constructed by the deferred coordinator-ingress adapter and tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowEventSinkError {
    /// The downstream boundary rejected an allegedly normalized event.
    Invalid,
    /// The bounded downstream queue cannot accept another event.
    Full,
    /// The downstream event relay is no longer available.
    Closed,
}

/// Object-safe boundary for already-normalized window events.
///
/// Implementations must return immediately and must not assign the public
/// event sequence. The coordinator remains the only sequence owner.
pub(crate) trait WindowEventSink: Send + Sync + 'static {
    /// Returns whether normalization work should be performed for this sink.
    fn enabled(&self) -> bool {
        true
    }

    /// Attempts to publish one validated additive-safe event without waiting.
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError>;

    /// Requests a downstream resynchronization barrier after any dropped event.
    fn require_resync(&self) {}
}

/// Default observation composition intentionally emits nowhere.
struct UnavailableWindowEventSink;

impl WindowEventSink for UnavailableWindowEventSink {
    fn enabled(&self) -> bool {
        false
    }

    fn try_emit(&self, _: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        Ok(())
    }
}

/// Saturating diagnostic counters for normalized events dropped before ingress.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WindowEventDropStats {
    /// Drops because the downstream bounded queue was full.
    pub(crate) full: u64,
    /// Drops because the downstream relay was closed.
    pub(crate) closed: u64,
    /// Drops because an internal DTO or serialized payload failed validation.
    pub(crate) invalid: u64,
}

#[derive(Default)]
struct WindowEventDeliveryMetrics {
    full: AtomicU64,
    closed: AtomicU64,
    invalid: AtomicU64,
}

impl WindowEventDeliveryMetrics {
    fn snapshot(&self) -> WindowEventDropStats {
        WindowEventDropStats {
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
        }
    }

    fn increment(counter: &AtomicU64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(1))
        });
    }
}

#[derive(Clone, Copy)]
struct WindowEventScope {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_revision: WindowModelRevision,
}

struct WindowEventEmitter {
    sink: Arc<dyn WindowEventSink>,
    metrics: Arc<WindowEventDeliveryMetrics>,
}

impl WindowEventEmitter {
    fn new(sink: Arc<dyn WindowEventSink>, metrics: Arc<WindowEventDeliveryMetrics>) -> Self {
        Self { sink, metrics }
    }

    fn enabled(&self) -> bool {
        self.sink.enabled()
    }

    fn require_resync(&self) {
        if self.sink.enabled() {
            self.sink.require_resync();
        }
    }

    fn offer(&self, event: Option<NormalizedEvent>) {
        if !self.sink.enabled() {
            return;
        }
        let Some(event) = event else {
            WindowEventDeliveryMetrics::increment(&self.metrics.invalid);
            self.sink.require_resync();
            return;
        };
        match self.sink.try_emit(event) {
            Ok(()) => {}
            Err(WindowEventSinkError::Invalid) => {
                WindowEventDeliveryMetrics::increment(&self.metrics.invalid);
                self.sink.require_resync();
            }
            Err(WindowEventSinkError::Full) => {
                WindowEventDeliveryMetrics::increment(&self.metrics.full);
                self.sink.require_resync();
            }
            Err(WindowEventSinkError::Closed) => {
                WindowEventDeliveryMetrics::increment(&self.metrics.closed);
                self.sink.require_resync();
            }
        }
    }

    fn emit_committed_transition(
        &self,
        before: &CommittedWindowModelView,
        after: &CommittedWindowModelView,
        policy: ReconcileEventPolicy,
    ) {
        if !self.sink.enabled() {
            return;
        }
        if policy == ReconcileEventPolicy::InitialBaseline {
            self.offer(model_rebuilt_event(
                before,
                after,
                WindowModelRebuildReason::Startup,
                true,
            ));
            return;
        }

        // An XID reincarnation is two exact-identity transitions. Publishing
        // every old birth first ensures a consumer can never interpret the
        // successor as an in-place update of a stale reference.
        for (xid, previous) in &before.windows {
            if after
                .windows
                .get(xid)
                .is_none_or(|current| current.window != previous.window)
            {
                self.offer(lifecycle_event(
                    previous.window.clone(),
                    after.revision,
                    WindowLifecycleKind::Destroyed,
                    WINDOW_DESTROYED_TOPIC,
                ));
            }
        }
        for (xid, current) in &after.windows {
            if before
                .windows
                .get(xid)
                .is_none_or(|previous| previous.window != current.window)
            {
                self.offer(lifecycle_event(
                    current.window.clone(),
                    after.revision,
                    WindowLifecycleKind::Created,
                    WINDOW_CREATED_TOPIC,
                ));
            }
        }

        for (xid, current) in &after.windows {
            let Some(previous) = before
                .windows
                .get(xid)
                .filter(|previous| previous.window == current.window)
            else {
                continue;
            };
            let changed = metadata_changes(previous, current);
            if !changed.is_empty() {
                self.offer(metadata_event(current, changed, after.revision));
            }
        }

        let (previous_active, previous_focused) = focus_projection(&before.windows);
        let (active, focused) = focus_projection(&after.windows);
        if previous_active != active || previous_focused != focused {
            self.offer(focus_event(
                after,
                previous_active,
                active,
                previous_focused,
                focused,
            ));
        }

        for (xid, current) in &after.windows {
            let Some(previous) = before
                .windows
                .get(xid)
                .filter(|previous| previous.window == current.window)
            else {
                continue;
            };
            if previous.geometry != current.geometry {
                self.offer(geometry_event(previous, current, after.revision));
            }
        }

        if let ReconcileEventPolicy::Rebuilt(reason) = policy {
            self.offer(model_rebuilt_event(before, after, reason, false));
        }
    }

    fn emit_committed_observation(
        &self,
        scope: WindowEventScope,
        previous_active: Option<WindowRef>,
        previous_focused: Option<WindowRef>,
        previous: Option<&WindowSnapshot>,
        current: &WindowSnapshot,
    ) {
        if !self.enabled() {
            return;
        }
        if let Some(previous) = previous {
            let changed = metadata_changes(previous, current);
            if !changed.is_empty() {
                self.offer(metadata_event(current, changed, scope.model_revision));
            }
        } else {
            self.offer(lifecycle_event(
                current.window.clone(),
                scope.model_revision,
                WindowLifecycleKind::Created,
                WINDOW_CREATED_TOPIC,
            ));
        }

        let active = if current.state.active {
            Some(current.window.clone())
        } else {
            previous_active
                .as_ref()
                .filter(|window| window.xid != current.window.xid)
                .cloned()
        };
        let focused = if current.state.focused {
            Some(current.window.clone())
        } else {
            previous_focused
                .as_ref()
                .filter(|window| window.xid != current.window.xid)
                .cloned()
        };
        if previous_active != active || previous_focused != focused {
            self.offer(focus_event_for_scope(
                scope,
                previous_active,
                active,
                previous_focused,
                focused,
            ));
        }
        if let Some(previous) = previous
            && previous.geometry != current.geometry
        {
            self.offer(geometry_event(previous, current, scope.model_revision));
        }
    }

    fn emit_committed_removal(
        &self,
        scope: WindowEventScope,
        previous_active: Option<WindowRef>,
        previous_focused: Option<WindowRef>,
        previous: &WindowSnapshot,
    ) {
        if !self.enabled() {
            return;
        }
        self.offer(lifecycle_event(
            previous.window.clone(),
            scope.model_revision,
            WindowLifecycleKind::Destroyed,
            WINDOW_DESTROYED_TOPIC,
        ));
        let active = previous_active
            .as_ref()
            .filter(|window| window.xid != previous.window.xid)
            .cloned();
        let focused = previous_focused
            .as_ref()
            .filter(|window| window.xid != previous.window.xid)
            .cloned();
        if previous_active != active || previous_focused != focused {
            self.offer(focus_event_for_scope(
                scope,
                previous_active,
                active,
                previous_focused,
                focused,
            ));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileEventPolicy {
    Incremental,
    /// Initial inventory is one baseline rebuild, never a created-event burst.
    InitialBaseline,
    Rebuilt(WindowModelRebuildReason),
}

struct CommittedWindowModelView {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    revision: WindowModelRevision,
    windows: BTreeMap<u32, WindowSnapshot>,
}

impl CommittedWindowModelView {
    fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        revision: WindowModelRevision,
        snapshots: Vec<WindowSnapshot>,
    ) -> Self {
        Self {
            desktop_id,
            desktop_generation,
            revision,
            windows: snapshots
                .into_iter()
                .map(|snapshot| (snapshot.window.xid, snapshot))
                .collect(),
        }
    }
}

fn process_correlation_universe_changed(
    before: &CommittedWindowModelView,
    after: &CommittedWindowModelView,
) -> bool {
    if before.windows.len() != after.windows.len() {
        return true;
    }
    before.windows.iter().any(|(xid, previous)| {
        after.windows.get(xid).is_none_or(|current| {
            previous.window != current.window
                || previous.process.reported_pid != current.process.reported_pid
                || previous.process.managed_process != current.process.managed_process
                || previous.process.confidence != current.process.confidence
                || previous.process.evidence != current.process.evidence
                || previous.process.conflict != current.process.conflict
        })
    })
}

fn lifecycle_event(
    window: WindowRef,
    model_revision: WindowModelRevision,
    lifecycle: WindowLifecycleKind,
    topic: &str,
) -> Option<NormalizedEvent> {
    let payload = WindowLifecycleEvent {
        window,
        model_revision,
        lifecycle,
    };
    payload.validate().ok()?;
    normalized_window_event(topic, serde_json::to_value(payload).ok()?)
}

fn metadata_event(
    snapshot: &WindowSnapshot,
    changed: Vec<WindowMetadataField>,
    model_revision: WindowModelRevision,
) -> Option<NormalizedEvent> {
    let payload = WindowMetadataEvent {
        window: snapshot.window.clone(),
        model_revision,
        changed,
        metadata: snapshot.metadata.clone(),
    };
    payload.validate().ok()?;
    normalized_window_event(WINDOW_CHANGED_TOPIC, serde_json::to_value(payload).ok()?)
}

fn focus_event(
    after: &CommittedWindowModelView,
    previous_active: Option<WindowRef>,
    active: Option<WindowRef>,
    previous_focused: Option<WindowRef>,
    focused: Option<WindowRef>,
) -> Option<NormalizedEvent> {
    focus_event_for_scope(
        WindowEventScope {
            desktop_id: after.desktop_id,
            desktop_generation: after.desktop_generation,
            model_revision: after.revision,
        },
        previous_active,
        active,
        previous_focused,
        focused,
    )
}

fn focus_event_for_scope(
    scope: WindowEventScope,
    previous_active: Option<WindowRef>,
    active: Option<WindowRef>,
    previous_focused: Option<WindowRef>,
    focused: Option<WindowRef>,
) -> Option<NormalizedEvent> {
    let payload = WindowFocusEvent {
        desktop_id: scope.desktop_id,
        desktop_generation: scope.desktop_generation,
        model_revision: scope.model_revision,
        previous_active,
        active,
        previous_focused,
        focused,
    };
    payload.validate().ok()?;
    normalized_window_event(WINDOW_CHANGED_TOPIC, serde_json::to_value(payload).ok()?)
}

fn geometry_event(
    previous: &WindowSnapshot,
    current: &WindowSnapshot,
    model_revision: WindowModelRevision,
) -> Option<NormalizedEvent> {
    let payload = WindowGeometryEvent {
        window: current.window.clone(),
        model_revision,
        before: previous.geometry.clone(),
        after: current.geometry.clone()?,
    };
    payload.validate().ok()?;
    normalized_window_event(WINDOW_CHANGED_TOPIC, serde_json::to_value(payload).ok()?)
}

fn model_rebuilt_event(
    before: &CommittedWindowModelView,
    after: &CommittedWindowModelView,
    reason: WindowModelRebuildReason,
    initial: bool,
) -> Option<NormalizedEvent> {
    let payload = WindowModelRebuiltEvent {
        desktop_id: after.desktop_id,
        desktop_generation: after.desktop_generation,
        previous_revision: (!initial && before.revision < after.revision)
            .then_some(before.revision),
        model_revision: after.revision,
        window_count: u32::try_from(after.windows.len()).ok()?,
        reason,
    };
    payload.validate().ok()?;
    normalized_window_event(
        WINDOW_MODEL_REBUILT_TOPIC,
        serde_json::to_value(payload).ok()?,
    )
}

fn normalized_window_event(topic: &str, payload: serde_json::Value) -> Option<NormalizedEvent> {
    NormalizedEvent::new(EventTopic::new(topic).ok()?, payload).ok()
}

fn screen_damage_event(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    damage: RootDamageBatch,
) -> Option<NormalizedEvent> {
    let payload = ScreenDamageEvent {
        desktop_id,
        desktop_generation,
        root_region: root_damage_rect(damage.root_region)?,
        damaged_regions: damage
            .regions
            .into_iter()
            .map(root_damage_rect)
            .collect::<Option<Vec<_>>>()?,
        coverage: match damage.coverage {
            RootDamageCoverage::Regions => ScreenDamageCoverage::Regions,
            RootDamageCoverage::BoundingBox => ScreenDamageCoverage::BoundingBox,
            RootDamageCoverage::FullScreen => ScreenDamageCoverage::FullScreen,
        },
        coalesced_notifications: damage.notifications,
    };
    payload.validate().ok()?;
    normalized_window_event(SCREEN_DAMAGED_TOPIC, serde_json::to_value(payload).ok()?)
}

fn root_damage_rect(rect: RootDamageRect) -> Option<WindowRect> {
    WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(rect.x(), rect.y(), rect.width(), rect.height()).ok()?,
    )
    .ok()
}

fn focus_projection(
    windows: &BTreeMap<u32, WindowSnapshot>,
) -> (Option<WindowRef>, Option<WindowRef>) {
    let active = windows
        .values()
        .find(|snapshot| snapshot.state.active)
        .map(|snapshot| snapshot.window.clone());
    let focused = windows
        .values()
        .find(|snapshot| snapshot.state.focused)
        .map(|snapshot| snapshot.window.clone());
    (active, focused)
}

fn metadata_changes(
    previous: &WindowSnapshot,
    current: &WindowSnapshot,
) -> Vec<WindowMetadataField> {
    let mut changed = Vec::new();
    if previous.metadata.title != current.metadata.title {
        changed.push(WindowMetadataField::Title);
    }
    if previous.metadata.visible_title != current.metadata.visible_title {
        changed.push(WindowMetadataField::VisibleTitle);
    }
    if previous.metadata.icon_title != current.metadata.icon_title {
        changed.push(WindowMetadataField::IconTitle);
    }
    if previous.metadata.class != current.metadata.class {
        changed.push(WindowMetadataField::Class);
    }
    if previous.metadata.client_machine != current.metadata.client_machine {
        changed.push(WindowMetadataField::ClientMachine);
    }
    if previous.metadata.window_types != current.metadata.window_types {
        changed.push(WindowMetadataField::WindowTypes);
    }
    if previous.metadata.states != current.metadata.states
        || non_focus_state_changed(&previous.state, &current.state)
    {
        changed.push(WindowMetadataField::States);
    }
    if previous.metadata.allowed_actions != current.metadata.allowed_actions {
        changed.push(WindowMetadataField::AllowedActions);
    }
    if previous.metadata.protocols != current.metadata.protocols {
        changed.push(WindowMetadataField::Protocols);
    }
    if previous.workspace != current.workspace {
        changed.push(WindowMetadataField::Workspace);
    }
    if previous.client_leader != current.client_leader
        || previous.transient_for != current.transient_for
        || previous.group_leader != current.group_leader
    {
        changed.push(WindowMetadataField::Relationships);
    }
    if previous.process != current.process {
        changed.push(WindowMetadataField::ProcessCorrelation);
    }
    if previous.has_accessibility_application != current.has_accessibility_application {
        changed.push(WindowMetadataField::AccessibilityCorrelation);
    }
    changed
}

fn non_focus_state_changed(previous: &WindowObservedState, current: &WindowObservedState) -> bool {
    previous.map_state != current.map_state
        || previous.minimized != current.minimized
        || previous.hidden != current.hidden
        || previous.urgent != current.urgent
        || previous.modal != current.modal
        || previous.sticky != current.sticky
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationAdapterError {
    InvalidRawObservation,
    IdentityEncoding,
    TokenUnavailable,
    ClockOverflow,
    Model,
}

impl fmt::Display for ObservationAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRawObservation => "raw window observation was invalid",
            Self::IdentityEncoding => "window identity could not be encoded",
            Self::TokenUnavailable => "opaque window token was unavailable",
            Self::ClockOverflow => "observation deadline overflowed",
            Self::Model => "window model invariant failed",
        })
    }
}

impl Error for ObservationAdapterError {}

fn validate_raw_input(input: &WindowSnapshotInput) -> Result<(), ObservationAdapterError> {
    let properties = &input.properties;
    if input.window == 0
        || input.geometry.geometry_root == 0
        || input.geometry.client_rect.coordinate_space != CoordinateSpace::RootPhysical
        || input.geometry.client_rect.validate().is_err()
        || properties.reported_pid == Some(0)
        || [
            properties.client_leader,
            properties.transient_for,
            properties.group_leader,
        ]
        .into_iter()
        .flatten()
        .any(|xid| xid == 0)
        || [
            input.root.active_window,
            input.root.raw_focused_window,
            input.root.focused_window,
        ]
        .into_iter()
        .flatten()
        .any(|xid| xid == 0)
        || (input.root.focus_ancestry_status == FocusAncestryStatus::NoFocus
            && (input.root.raw_focused_window.is_some()
                || input.root.focused_window.is_some()
                || input.root.target_contains_focus))
        || [
            properties.window_types.len(),
            properties.states.len(),
            properties.allowed_actions.len(),
            properties.protocols.len(),
        ]
        .into_iter()
        .any(|count| count > MAX_WINDOW_ATOMS)
        || [
            &properties.window_types,
            &properties.states,
            &properties.allowed_actions,
            &properties.protocols,
        ]
        .into_iter()
        .flatten()
        .any(|atom| atom.id == 0)
        || properties.warnings.len() > MAX_SNAPSHOT_INPUT_WARNINGS
    {
        return Err(ObservationAdapterError::InvalidRawObservation);
    }
    Ok(())
}

fn build_identity_hash(
    input: &WindowSnapshotInput,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    birth_serial: u64,
) -> Result<WindowIdentityHash, ObservationAdapterError> {
    validate_raw_input(input)?;
    let mut digest = Sha256::new();
    digest.update(IDENTITY_HASH_DOMAIN);
    hash_field(&mut digest, desktop_id.as_uuid().as_bytes());
    hash_field(&mut digest, desktop_generation.as_uuid().as_bytes());
    hash_field(&mut digest, &input.window.to_be_bytes());
    hash_field(&mut digest, &birth_serial.to_be_bytes());
    hash_field(&mut digest, &input.attributes.visual.to_be_bytes());
    hash_field(&mut digest, &input.attributes.colormap.to_be_bytes());
    hash_field(&mut digest, &[u8::from(input.attributes.override_redirect)]);
    hash_field(&mut digest, &[u8::from(input.attributes.input_only)]);
    hash_optional_u32(&mut digest, input.properties.reported_pid);
    hash_optional_u32(&mut digest, input.properties.client_leader);
    if let Some(class) = &input.properties.class {
        hash_optional_text(&mut digest, class.instance.as_ref());
        hash_optional_text(&mut digest, class.class.as_ref());
    } else {
        hash_field(&mut digest, &[]);
        hash_field(&mut digest, &[]);
    }
    let bytes: [u8; 32] = digest.finalize().into();
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}")
            .map_err(|_| ObservationAdapterError::IdentityEncoding)?;
    }
    WindowIdentityHash::new(encoded).map_err(|_| ObservationAdapterError::IdentityEncoding)
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

fn hash_optional_u32(digest: &mut Sha256, value: Option<u32>) {
    match value {
        Some(value) => hash_field(digest, &value.to_be_bytes()),
        None => hash_field(digest, &[]),
    }
}

fn hash_optional_text(digest: &mut Sha256, value: Option<&xenoteer_protocol::WindowText>) {
    match value {
        Some(value) => {
            hash_field(digest, value.value.as_bytes());
            hash_field(digest, &[u8::from(value.lossy)]);
        }
        None => hash_field(digest, &[]),
    }
}

fn normalize_snapshot(
    input: &WindowSnapshotInput,
    reference: WindowRef,
    model_revision: WindowModelRevision,
    stacking_index: Option<u32>,
    references: &BTreeMap<u32, WindowRef>,
) -> Result<WindowSnapshot, ObservationAdapterError> {
    validate_raw_input(input)?;
    if input.window != reference.xid {
        return Err(ObservationAdapterError::InvalidRawObservation);
    }
    let properties = &input.properties;
    let process = match properties.reported_pid {
        Some(pid) => WindowProcessCorrelation {
            reported_pid: Some(pid),
            managed_process: None,
            confidence: WindowProcessConfidence::Low,
            evidence: vec![WindowProcessEvidence::NetWmPid],
            conflict: false,
        },
        None => WindowProcessCorrelation {
            reported_pid: None,
            managed_process: None,
            confidence: WindowProcessConfidence::None,
            evidence: Vec::new(),
            conflict: false,
        },
    };
    let mut warnings = normalize_warnings(&properties.warnings)?;
    if properties.warnings_truncated {
        push_warning(
            &mut warnings,
            WindowSnapshotWarning::TruncatedProperty {
                property: WindowAtomName::new("observation_warnings")
                    .map_err(|_| ObservationAdapterError::InvalidRawObservation)?,
            },
        );
    }
    let frame_rect = properties.frame_extents.and_then(|extents| {
        push_warning(&mut warnings, WindowSnapshotWarning::FrameExtentsUnverified);
        match derive_frame_rect(input.geometry.client_rect, extents) {
            Ok(frame) => Some(frame),
            Err(()) => {
                push_warning(
                    &mut warnings,
                    WindowSnapshotWarning::FrameGeometryUnavailable,
                );
                None
            }
        }
    });
    let geometry = WindowGeometry {
        client_rect: input.geometry.client_rect,
        frame_rect,
        content_rect: input.geometry.client_rect,
        frame_extents: properties.frame_extents,
    };
    let window_types = normalize_atoms(
        &properties.window_types,
        KnownAtom::NetWmWindowType,
        &mut warnings,
    )?;
    let states = normalize_atoms(&properties.states, KnownAtom::NetWmState, &mut warnings)?;
    let allowed_actions = normalize_atoms(
        &properties.allowed_actions,
        KnownAtom::NetWmAllowedActions,
        &mut warnings,
    )?;
    let protocols = normalize_atoms(&properties.protocols, KnownAtom::WmProtocols, &mut warnings)?;
    let has_state = |needle| {
        properties
            .states
            .iter()
            .any(|atom| atom.known == Some(needle))
    };
    let hidden = has_state(KnownAtom::NetWmStateHidden);
    let snapshot = WindowSnapshot {
        xid_hex: reference.xid_hex(),
        window: reference,
        model_revision,
        metadata: WindowMetadata {
            title: properties.title.clone(),
            visible_title: properties.visible_title.clone(),
            icon_title: properties.icon_title.clone(),
            class: properties.class.clone(),
            client_machine: properties.client_machine.clone(),
            window_types,
            states,
            allowed_actions,
            protocols,
        },
        process,
        state: WindowObservedState {
            map_state: input.attributes.map_state,
            minimized: hidden,
            hidden,
            urgent: properties.urgent,
            modal: has_state(KnownAtom::NetWmStateModal),
            sticky: has_state(KnownAtom::NetWmStateSticky),
            active: input.root.active_window == Some(input.window),
            focused: (input.root.focus_ancestry_status == FocusAncestryStatus::Resolved
                && (input.root.target_contains_focus
                    || input.root.focused_window == Some(input.window)))
                || has_state(KnownAtom::NetWmStateFocused),
        },
        geometry: Some(geometry),
        workspace: properties.workspace,
        client_leader: relation(properties.client_leader, references),
        transient_for: relation(properties.transient_for, references),
        group_leader: relation(properties.group_leader, references),
        stacking_index,
        has_accessibility_application: false,
        warnings,
    };
    snapshot
        .validate()
        .map_err(|_| ObservationAdapterError::InvalidRawObservation)?;
    Ok(snapshot)
}

fn derive_frame_rect(
    client: xenoteer_protocol::WindowRect,
    extents: xenoteer_protocol::WindowFrameExtents,
) -> Result<xenoteer_protocol::WindowRect, ()> {
    let origin = client.rect.origin();
    let size = client.rect.size().map_err(|_| ())?;
    let x = i32::try_from(i64::from(origin.x()) - i64::from(extents.left)).map_err(|_| ())?;
    let y = i32::try_from(i64::from(origin.y()) - i64::from(extents.top)).map_err(|_| ())?;
    let width =
        u32::try_from(u64::from(size.width()) + u64::from(extents.left) + u64::from(extents.right))
            .map_err(|_| ())?;
    let height = u32::try_from(
        u64::from(size.height()) + u64::from(extents.top) + u64::from(extents.bottom),
    )
    .map_err(|_| ())?;
    let rect = xenoteer_protocol::Rect::new(x, y, width, height).map_err(|_| ())?;
    xenoteer_protocol::WindowRect::new(CoordinateSpace::RootPhysical, rect).map_err(|_| ())
}

fn normalize_atoms(
    values: &[ObservedAtom],
    source_property: KnownAtom,
    warnings: &mut Vec<WindowSnapshotWarning>,
) -> Result<Vec<WindowAtomName>, ObservationAdapterError> {
    let mut names = Vec::with_capacity(values.len());
    for value in values {
        let name = WindowAtomName::new(value.diagnostic_name())
            .map_err(|_| ObservationAdapterError::InvalidRawObservation)?;
        if !names.contains(&name) {
            names.push(name);
        }
        if value.known.is_none() {
            push_warning(
                warnings,
                WindowSnapshotWarning::UnsupportedPropertyEncoding {
                    property: known_atom_name(source_property)?,
                },
            );
        }
    }
    Ok(names)
}

fn relation(xid: Option<u32>, references: &BTreeMap<u32, WindowRef>) -> Option<WindowRef> {
    xid.and_then(|xid| references.get(&xid).cloned())
}

fn normalize_warnings(
    observed: &[ObservedPropertyWarning],
) -> Result<Vec<WindowSnapshotWarning>, ObservationAdapterError> {
    let mut warnings = Vec::new();
    for observed in observed {
        let property = known_atom_name(observed.property)?;
        let warning = match observed.warning {
            PropertyWarning::Malformed => WindowSnapshotWarning::MalformedProperty { property },
            PropertyWarning::Truncated => WindowSnapshotWarning::TruncatedProperty { property },
            PropertyWarning::LossyText => WindowSnapshotWarning::LossyPropertyText { property },
            PropertyWarning::UnexpectedType
            | PropertyWarning::UnexpectedFormat
            | PropertyWarning::UnknownAtom => {
                WindowSnapshotWarning::UnsupportedPropertyEncoding { property }
            }
        };
        push_warning(&mut warnings, warning);
    }
    Ok(warnings)
}

fn known_atom_name(atom: KnownAtom) -> Result<WindowAtomName, ObservationAdapterError> {
    let name = std::str::from_utf8(atom.name())
        .map_err(|_| ObservationAdapterError::InvalidRawObservation)?;
    WindowAtomName::new(name).map_err(|_| ObservationAdapterError::InvalidRawObservation)
}

fn push_warning(warnings: &mut Vec<WindowSnapshotWarning>, warning: WindowSnapshotWarning) {
    if warnings.len() < MAX_WINDOW_WARNINGS && !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CursorQueryBinding {
    List,
    Selector([u8; 32]),
}

impl CursorQueryBinding {
    fn from_descriptor(query: WindowContinuationQuery) -> Self {
        match query {
            WindowContinuationQuery::List => Self::List,
            WindowContinuationQuery::Selector { fingerprint } => {
                Self::Selector(*fingerprint.as_bytes())
            }
        }
    }
}

struct CursorClaim {
    principal: String,
    descriptor: WindowContinuationDescriptor,
    query: CursorQueryBinding,
    correlation_epoch: Option<u64>,
    expires_at: MonotonicMillis,
}

struct ReferenceClaim {
    principal: String,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    window: WindowRef,
    expires_at: MonotonicMillis,
}

struct OpaqueTokenRegistry {
    capacity: usize,
    cursor_ttl_ms: u64,
    reference_ttl_ms: u64,
    cursors: HashMap<[u8; 32], CursorClaim>,
    cursor_order: VecDeque<[u8; 32]>,
    references: HashMap<[u8; 32], ReferenceClaim>,
    reference_order: VecDeque<[u8; 32]>,
    entropy: Arc<dyn TokenEntropy>,
}

impl fmt::Debug for OpaqueTokenRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueTokenRegistry { <redacted> }")
    }
}

trait TokenEntropy: Send + Sync + 'static {
    fn fill(
        &self,
        destination: &mut [u8; TOKEN_SECRET_BYTES],
    ) -> Result<(), ObservationAdapterError>;
}

#[derive(Debug)]
struct SystemTokenEntropy;

impl TokenEntropy for SystemTokenEntropy {
    fn fill(
        &self,
        destination: &mut [u8; TOKEN_SECRET_BYTES],
    ) -> Result<(), ObservationAdapterError> {
        getrandom::fill(destination).map_err(|_| ObservationAdapterError::TokenUnavailable)
    }
}

impl OpaqueTokenRegistry {
    fn new(
        capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
    ) -> Result<Self, ObservationAdapterError> {
        Self::with_entropy(
            capacity,
            cursor_ttl_ms,
            reference_ttl_ms,
            Arc::new(SystemTokenEntropy),
        )
    }

    fn with_entropy(
        capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
        entropy: Arc<dyn TokenEntropy>,
    ) -> Result<Self, ObservationAdapterError> {
        if capacity == 0 || cursor_ttl_ms == 0 || reference_ttl_ms == 0 {
            return Err(ObservationAdapterError::TokenUnavailable);
        }
        Ok(Self {
            capacity,
            cursor_ttl_ms,
            reference_ttl_ms,
            cursors: HashMap::with_capacity(capacity.min(1_024)),
            cursor_order: VecDeque::with_capacity(capacity.min(1_024)),
            references: HashMap::with_capacity(capacity.min(1_024)),
            reference_order: VecDeque::with_capacity(capacity.min(1_024)),
            entropy,
        })
    }

    fn mint_cursor(
        &mut self,
        principal: &str,
        descriptor: WindowContinuationDescriptor,
        correlation_epoch: Option<u64>,
        now: MonotonicMillis,
    ) -> Result<WindowPageCursor, ObservationAdapterError> {
        self.purge(now);
        let expires_at = now
            .checked_add(self.cursor_ttl_ms)
            .ok_or(ObservationAdapterError::ClockOverflow)?;
        let query = CursorQueryBinding::from_descriptor(descriptor.query);
        let (token, digest) = mint_token::<WindowPageCursor, _>(
            "c",
            CURSOR_TOKEN_DOMAIN,
            self.entropy.as_ref(),
            |digest| self.cursors.contains_key(digest),
        )?;
        while self.cursors.len() >= self.capacity {
            self.evict_oldest_cursor();
        }
        self.cursors.insert(
            digest,
            CursorClaim {
                principal: principal.to_owned(),
                descriptor,
                query,
                correlation_epoch,
                expires_at,
            },
        );
        self.cursor_order.push_back(digest);
        Ok(token)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_cursor(
        &mut self,
        token: &WindowPageCursor,
        principal: &str,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        order: WindowOrder,
        query: CursorQueryBinding,
        correlation_epoch: Option<u64>,
        now: MonotonicMillis,
    ) -> Result<WindowContinuationDescriptor, ObservationAdapterError> {
        self.purge(now);
        let digest = token_digest(CURSOR_TOKEN_DOMAIN, token.as_str());
        let claim = self
            .cursors
            .get(&digest)
            .ok_or(ObservationAdapterError::TokenUnavailable)?;
        if claim.principal != principal
            || claim.descriptor.desktop_id != desktop_id
            || claim.descriptor.desktop_generation != desktop_generation
            || claim.descriptor.order != order
            || claim.query != query
            || claim.correlation_epoch != correlation_epoch
        {
            return Err(ObservationAdapterError::TokenUnavailable);
        }
        Ok(claim.descriptor.clone())
    }

    fn mint_reference(
        &mut self,
        principal: &str,
        window: WindowRef,
        now: MonotonicMillis,
    ) -> Result<WindowReferenceToken, ObservationAdapterError> {
        self.purge(now);
        let expires_at = now
            .checked_add(self.reference_ttl_ms)
            .ok_or(ObservationAdapterError::ClockOverflow)?;
        let (token, digest) = mint_token::<WindowReferenceToken, _>(
            "r",
            REFERENCE_TOKEN_DOMAIN,
            self.entropy.as_ref(),
            |digest| self.references.contains_key(digest),
        )?;
        while self.references.len() >= self.capacity {
            self.evict_oldest_reference();
        }
        self.references.insert(
            digest,
            ReferenceClaim {
                principal: principal.to_owned(),
                desktop_id: window.desktop_id,
                desktop_generation: window.desktop_generation,
                window,
                expires_at,
            },
        );
        self.reference_order.push_back(digest);
        Ok(token)
    }

    fn resolve_reference(
        &mut self,
        token: &WindowReferenceToken,
        principal: &str,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        now: MonotonicMillis,
    ) -> Result<WindowRef, ObservationAdapterError> {
        self.purge(now);
        let digest = token_digest(REFERENCE_TOKEN_DOMAIN, token.as_str());
        let claim = self
            .references
            .get(&digest)
            .ok_or(ObservationAdapterError::TokenUnavailable)?;
        if claim.principal != principal
            || claim.desktop_id != desktop_id
            || claim.desktop_generation != desktop_generation
        {
            return Err(ObservationAdapterError::TokenUnavailable);
        }
        Ok(claim.window.clone())
    }

    fn purge(&mut self, now: MonotonicMillis) {
        while self.cursor_order.front().is_some_and(|digest| {
            self.cursors
                .get(digest)
                .is_none_or(|claim| claim.expires_at <= now)
        }) {
            self.evict_oldest_cursor();
        }
        while self.reference_order.front().is_some_and(|digest| {
            self.references
                .get(digest)
                .is_none_or(|claim| claim.expires_at <= now)
        }) {
            self.evict_oldest_reference();
        }
    }

    fn evict_oldest_cursor(&mut self) {
        if let Some(digest) = self.cursor_order.pop_front() {
            self.cursors.remove(&digest);
        }
    }

    fn evict_oldest_reference(&mut self) {
        if let Some(digest) = self.reference_order.pop_front() {
            self.references.remove(&digest);
        }
    }
}

trait OpaqueProtocolToken: Sized {
    fn from_opaque(value: String) -> Result<Self, ObservationAdapterError>;
}

impl OpaqueProtocolToken for WindowPageCursor {
    fn from_opaque(value: String) -> Result<Self, ObservationAdapterError> {
        Self::new(value).map_err(|_| ObservationAdapterError::TokenUnavailable)
    }
}

impl OpaqueProtocolToken for WindowReferenceToken {
    fn from_opaque(value: String) -> Result<Self, ObservationAdapterError> {
        Self::new(value).map_err(|_| ObservationAdapterError::TokenUnavailable)
    }
}

fn mint_token<T: OpaqueProtocolToken, F: Fn(&[u8; 32]) -> bool>(
    prefix: &str,
    domain: &[u8],
    entropy: &dyn TokenEntropy,
    digest_exists: F,
) -> Result<(T, [u8; 32]), ObservationAdapterError> {
    for _ in 0..MAX_TOKEN_MINT_ATTEMPTS {
        let mut secret = [0_u8; TOKEN_SECRET_BYTES];
        entropy.fill(&mut secret)?;
        let mut value = String::with_capacity(prefix.len() + 1 + TOKEN_SECRET_BYTES * 2);
        value.push_str(prefix);
        value.push('_');
        for byte in secret {
            write!(&mut value, "{byte:02x}")
                .map_err(|_| ObservationAdapterError::TokenUnavailable)?;
        }
        secret.fill(0);
        let digest = token_digest(domain, &value);
        if !digest_exists(&digest) {
            return Ok((T::from_opaque(value)?, digest));
        }
    }
    Err(ObservationAdapterError::TokenUnavailable)
}

fn token_digest(domain: &[u8], token: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(token.as_bytes());
    digest.finalize().into()
}

fn selector_binding(
    selector: &WindowSelector,
) -> Result<CursorQueryBinding, ObservationAdapterError> {
    let encoded =
        serde_json::to_vec(selector).map_err(|_| ObservationAdapterError::TokenUnavailable)?;
    let mut digest = Sha256::new();
    digest.update(SELECTOR_FINGERPRINT_DOMAIN);
    digest.update(encoded);
    Ok(CursorQueryBinding::Selector(digest.finalize().into()))
}

fn selector_requires_managed_process(selector: &WindowSelector) -> bool {
    match selector {
        WindowSelector::Predicate {
            predicate: WindowPredicate::ManagedProcess { .. },
        } => true,
        WindowSelector::Predicate { .. } => false,
        WindowSelector::All { selectors } | WindowSelector::Any { selectors } => {
            selectors.iter().any(selector_requires_managed_process)
        }
        WindowSelector::Not { selector } => selector_requires_managed_process(selector),
    }
}

fn wait_requires_managed_process(request: &WindowWaitRequest) -> bool {
    match &request.target {
        WindowWaitTarget::Reference { .. } => false,
        WindowWaitTarget::Selector { selector, .. } => selector_requires_managed_process(selector),
    }
}

struct ModelState {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    max_live_windows: usize,
    model: WindowModel,
    tokens: OpaqueTokenRegistry,
    events: WindowEventEmitter,
    stacking_epoch: u64,
    correlation_epoch: u64,
    process_correlation_available: bool,
    model_changes: Option<Arc<ProcessCorrelationModelChanges>>,
    process_lifecycle_authority: Arc<ProcessLifecycleAuthority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileIdentityPolicy {
    PreserveContinuity,
    InvalidateAll,
}

impl ModelState {
    #[cfg(test)]
    fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        model_limits: WindowModelLimits,
        token_capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
    ) -> Result<Self, ObservationAdapterError> {
        Self::new_with_event_components(
            desktop_id,
            desktop_generation,
            model_limits,
            token_capacity,
            cursor_ttl_ms,
            reference_ttl_ms,
            Arc::new(UnavailableWindowEventSink),
            Arc::new(WindowEventDeliveryMetrics::default()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_event_components(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        model_limits: WindowModelLimits,
        token_capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
        event_sink: Arc<dyn WindowEventSink>,
        event_metrics: Arc<WindowEventDeliveryMetrics>,
    ) -> Result<Self, ObservationAdapterError> {
        let max_live_windows = model_limits.max_live_windows;
        Ok(Self {
            desktop_id,
            desktop_generation,
            max_live_windows,
            model: WindowModel::new(desktop_id, desktop_generation, model_limits)
                .map_err(|_| ObservationAdapterError::Model)?,
            tokens: OpaqueTokenRegistry::new(token_capacity, cursor_ttl_ms, reference_ttl_ms)?,
            events: WindowEventEmitter::new(event_sink, event_metrics),
            stacking_epoch: 0,
            correlation_epoch: 1,
            process_correlation_available: false,
            model_changes: None,
            process_lifecycle_authority: Arc::new(ProcessLifecycleAuthority::new()),
        })
    }

    #[cfg(test)]
    fn new_with_event_sink(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        model_limits: WindowModelLimits,
        token_capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
        event_sink: Arc<dyn WindowEventSink>,
    ) -> Result<Self, ObservationAdapterError> {
        Self::new_with_event_components(
            desktop_id,
            desktop_generation,
            model_limits,
            token_capacity,
            cursor_ttl_ms,
            reference_ttl_ms,
            event_sink,
            Arc::new(WindowEventDeliveryMetrics::default()),
        )
    }

    #[cfg(test)]
    fn event_drop_stats(&self) -> WindowEventDropStats {
        self.events.metrics.snapshot()
    }

    fn emit_root_damage(&self, damage: RootDamageBatch) {
        self.events.offer(screen_damage_event(
            self.desktop_id,
            self.desktop_generation,
            damage,
        ));
    }

    #[cfg(test)]
    fn reconcile_raw(
        &mut self,
        inventory: &RootInventory,
        inputs: &[WindowSnapshotInput],
        now: MonotonicMillis,
        identity_policy: ReconcileIdentityPolicy,
    ) -> Result<(), ObservationAdapterError> {
        self.reconcile_raw_with_event_policy(
            inventory,
            inputs,
            now,
            identity_policy,
            ReconcileEventPolicy::Incremental,
        )
    }

    fn reconcile_raw_with_event_policy(
        &mut self,
        inventory: &RootInventory,
        inputs: &[WindowSnapshotInput],
        now: MonotonicMillis,
        identity_policy: ReconcileIdentityPolicy,
        event_policy: ReconcileEventPolicy,
    ) -> Result<(), ObservationAdapterError> {
        validate_inventory(inventory, inputs)?;
        if inputs.len() > self.max_live_windows {
            return Err(ObservationAdapterError::Model);
        }
        let (current_revision, current) = self
            .model
            .snapshot_all(now)
            .map_err(|_| ObservationAdapterError::Model)?;
        let before = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            current_revision,
            current,
        );
        let current_references = before
            .windows
            .values()
            .map(|snapshot| (snapshot.window.xid, snapshot.window.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut desired_references = BTreeMap::new();
        let mut next_birth = self
            .model
            .next_birth_serial()
            .map_err(|_| ObservationAdapterError::Model)?;
        for input in inputs {
            let reference = if identity_policy == ReconcileIdentityPolicy::PreserveContinuity
                && let Some(existing) = current_references.get(&input.window)
            {
                existing.clone()
            } else {
                let reference = WindowRef {
                    desktop_id: self.desktop_id,
                    desktop_generation: self.desktop_generation,
                    xid: input.window,
                    observed_generation: next_birth,
                    identity_hash: build_identity_hash(
                        input,
                        self.desktop_id,
                        self.desktop_generation,
                        next_birth,
                    )?,
                };
                next_birth = next_birth
                    .checked_add(1)
                    .ok_or(ObservationAdapterError::Model)?;
                reference
            };
            desired_references.insert(input.window, reference);
        }
        let has_stacking_order = inventory.source == InventorySource::NetClientListStacking;
        let mut normalized = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let reference = desired_references
                .get(&input.window)
                .cloned()
                .ok_or(ObservationAdapterError::InvalidRawObservation)?;
            let mut snapshot = normalize_snapshot(
                input,
                reference,
                self.model.revision(),
                has_stacking_order
                    .then(|| u32::try_from(index).map_err(|_| ObservationAdapterError::Model))
                    .transpose()?,
                &desired_references,
            )?;
            if let Some(previous) = before.windows.get(&input.window)
                && previous.window == snapshot.window
                && previous.process.reported_pid == snapshot.process.reported_pid
            {
                // Broker evidence belongs to one exact X11 birth and reported
                // PID. Preserve it across unrelated metadata/geometry
                // reconciliation, but never across PID mutation or XID reuse.
                snapshot.process = previous.process.clone();
            }
            snapshot.has_accessibility_application =
                before.windows.get(&input.window).is_some_and(|current| {
                    current.window == snapshot.window && current.has_accessibility_application
                });
            normalized.push(snapshot);
        }
        for (xid, reference) in current_references {
            if identity_policy == ReconcileIdentityPolicy::InvalidateAll
                || !desired_references.contains_key(&xid)
            {
                self.model
                    .destroy(&reference, now)
                    .map_err(|_| ObservationAdapterError::Model)?;
            }
        }
        let mut committed = Vec::with_capacity(normalized.len());
        for snapshot in normalized {
            let change = self
                .model
                .observe(snapshot, now)
                .map_err(|_| ObservationAdapterError::Model)?;
            committed.push(match change {
                WindowModelChange::Created(snapshot) | WindowModelChange::Updated(snapshot) => {
                    snapshot
                }
            });
        }
        let after = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            self.model.revision(),
            committed,
        );
        if process_correlation_universe_changed(&before, &after) {
            self.advance_correlation_epoch()?;
            self.process_correlation_available = false;
        }
        self.events
            .emit_committed_transition(&before, &after, event_policy);
        Ok(())
    }

    fn advance_correlation_epoch(&mut self) -> Result<(), ObservationAdapterError> {
        self.correlation_epoch = self
            .correlation_epoch
            .checked_add(1)
            .ok_or(ObservationAdapterError::Model)?;
        Ok(())
    }

    fn publish_model_change(&self) -> Option<u64> {
        if let Some(changes) = &self.model_changes {
            return Some(changes.publish());
        }
        None
    }

    fn model_change_sequence(&self) -> u64 {
        self.model_changes
            .as_ref()
            .map_or(0, |changes| changes.sequence())
    }

    fn replace_process_correlations(
        &mut self,
        expected_revision: WindowModelRevision,
        expected_correlation_epoch: u64,
        expected_generation: DesktopGeneration,
        assignments: &[ProcessCorrelationAssignment],
        now: MonotonicMillis,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        if expected_generation != self.desktop_generation {
            return Err(ControlPlaneError::PermissionDenied);
        }
        if assignments.len() > self.max_live_windows {
            return Err(ControlPlaneError::ResourceExhausted);
        }
        let (before_revision, current) = self.model.snapshot_all(now).map_err(map_model_error)?;
        if before_revision != expected_revision
            || self.correlation_epoch != expected_correlation_epoch
        {
            return Err(ControlPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        if assignments.len() != current.len() {
            return Err(ControlPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        let before = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            before_revision,
            current.clone(),
        );
        let mut by_xid = BTreeMap::new();
        for assignment in assignments {
            if assignment.window.validate_shape().is_err()
                || assignment.window.desktop_id != self.desktop_id
                || assignment.window.desktop_generation != self.desktop_generation
                || by_xid.insert(assignment.window.xid, assignment).is_some()
            {
                return Err(ControlPlaneError::InvalidRequest);
            }
        }

        let mut desired = Vec::with_capacity(current.len());
        for mut snapshot in current {
            let assignment = by_xid.get(&snapshot.window.xid).copied().ok_or(
                ControlPlaneError::StaleReference {
                    current_generation: Some(self.desktop_generation),
                },
            )?;
            if assignment.window != snapshot.window
                || assignment.reported_pid != snapshot.process.reported_pid
            {
                return Err(ControlPlaneError::StaleReference {
                    current_generation: Some(self.desktop_generation),
                });
            }
            snapshot.process =
                process_correlation_from_assignment(assignment, self.desktop_generation)?;
            snapshot
                .validate()
                .map_err(|_| ControlPlaneError::InvalidRequest)?;
            desired.push(snapshot);
        }

        let changed = desired.iter().any(|snapshot| {
            before
                .windows
                .get(&snapshot.window.xid)
                .is_none_or(|previous| previous.process != snapshot.process)
        });
        let availability_changed = !self.process_correlation_available;
        if !changed && !availability_changed {
            return Ok(ProcessCorrelationFence {
                revision: before_revision,
                lifecycle_authority: self.process_lifecycle_authority.capture(),
            });
        }
        if changed {
            for snapshot in desired {
                self.model.observe(snapshot, now).map_err(map_model_error)?;
            }
        }
        self.process_correlation_available = true;
        self.correlation_epoch = self
            .correlation_epoch
            .checked_add(1)
            .ok_or(ControlPlaneError::Internal)?;
        let (after_revision, after_windows) =
            self.model.snapshot_all(now).map_err(map_model_error)?;
        let after = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            after_revision,
            after_windows,
        );
        self.events
            .emit_committed_transition(&before, &after, ReconcileEventPolicy::Incremental);
        self.publish_model_change();
        Ok(ProcessCorrelationFence {
            revision: after_revision,
            lifecycle_authority: self.process_lifecycle_authority.capture(),
        })
    }

    fn invalidate_process_correlations(
        &mut self,
        now: MonotonicMillis,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        let (before_revision, current) = self.model.snapshot_all(now).map_err(map_model_error)?;
        let before = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            before_revision,
            current.clone(),
        );
        let mut changed = false;
        for mut snapshot in current {
            let baseline = baseline_process_correlation(snapshot.process.reported_pid);
            if snapshot.process == baseline {
                continue;
            }
            snapshot.process = baseline;
            self.model.observe(snapshot, now).map_err(map_model_error)?;
            changed = true;
        }
        let availability_changed = self.process_correlation_available;
        if changed || availability_changed {
            self.correlation_epoch = self
                .correlation_epoch
                .checked_add(1)
                .ok_or(ControlPlaneError::Internal)?;
        }
        self.process_correlation_available = false;
        let revision = self.model.revision();
        if changed {
            let (_, after_windows) = self.model.snapshot_all(now).map_err(map_model_error)?;
            let after = CommittedWindowModelView::new(
                self.desktop_id,
                self.desktop_generation,
                revision,
                after_windows,
            );
            self.events.emit_committed_transition(
                &before,
                &after,
                ReconcileEventPolicy::Incremental,
            );
        }
        if changed || availability_changed {
            self.publish_model_change();
        }
        Ok(ProcessCorrelationFence {
            revision,
            lifecycle_authority: self.process_lifecycle_authority.capture(),
        })
    }

    fn replace_accessibility_correlations(
        &mut self,
        expected_revision: WindowModelRevision,
        windows: &[WindowRef],
        now: MonotonicMillis,
    ) -> Result<WindowModelRevision, ControlPlaneError> {
        if windows.len() > self.max_live_windows {
            return Err(ControlPlaneError::ResourceExhausted);
        }
        let (before_revision, current) = self.model.snapshot_all(now).map_err(map_model_error)?;
        if before_revision != expected_revision {
            return Err(ControlPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        let before = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            before_revision,
            current.clone(),
        );
        let current_by_xid = current
            .iter()
            .map(|snapshot| (snapshot.window.xid, &snapshot.window))
            .collect::<BTreeMap<_, _>>();
        let mut desired = BTreeMap::new();
        for window in windows {
            if window.validate_shape().is_err()
                || window.desktop_id != self.desktop_id
                || window.desktop_generation != self.desktop_generation
                || current_by_xid.get(&window.xid).copied() != Some(window)
                || desired.insert(window.xid, window).is_some()
            {
                return Err(ControlPlaneError::NotFound);
            }
        }

        let mut changed = false;
        for mut snapshot in current {
            let correlated = desired
                .get(&snapshot.window.xid)
                .is_some_and(|window| **window == snapshot.window);
            if snapshot.has_accessibility_application == correlated {
                continue;
            }
            snapshot.has_accessibility_application = correlated;
            self.model.observe(snapshot, now).map_err(map_model_error)?;
            changed = true;
        }
        if !changed {
            return Ok(before_revision);
        }

        let (after_revision, after_windows) =
            self.model.snapshot_all(now).map_err(map_model_error)?;
        let after = CommittedWindowModelView::new(
            self.desktop_id,
            self.desktop_generation,
            after_revision,
            after_windows,
        );
        self.events
            .emit_committed_transition(&before, &after, ReconcileEventPolicy::Incremental);
        self.publish_model_change();
        Ok(after_revision)
    }

    fn correlation_snapshot(
        &mut self,
        now: MonotonicMillis,
    ) -> Result<ObservationCorrelationSnapshot, ControlPlaneError> {
        let (revision, windows) = self.model.snapshot_all(now).map_err(map_model_error)?;
        if windows.len() > MAX_ACCESSIBILITY_CORRELATION_CANDIDATES {
            return Err(ControlPlaneError::ResourceExhausted);
        }
        Ok(ObservationCorrelationSnapshot {
            revision,
            correlation_epoch: self.correlation_epoch,
            observed_at: now,
            windows,
        })
    }

    fn occlusion_snapshot(
        &mut self,
        target_window: &WindowRef,
        now: MonotonicMillis,
    ) -> Result<WindowOcclusionSnapshot, ControlPlaneError> {
        let target = self
            .model
            .resolve_exact(target_window, now)
            .map_err(map_model_error)?
            .snapshot;
        let target_stacking_index = target.stacking_index;
        let target_client_bounds = target
            .geometry
            .as_ref()
            .map(|geometry| geometry.client_rect.rect);
        let (model_revision, windows) = self.model.snapshot_all(now).map_err(map_model_error)?;

        let mut stacking_complete = target_stacking_index.is_some()
            && target_client_bounds.is_some()
            && target.state.map_state == xenoteer_protocol::WindowMapState::Viewable
            && !target.state.hidden
            && !target.state.minimized;
        let mut seen_indices = BTreeMap::new();
        let mut rectangles = Vec::new();
        for snapshot in windows {
            if snapshot.state.map_state != xenoteer_protocol::WindowMapState::Viewable
                || snapshot.state.hidden
                || snapshot.state.minimized
            {
                continue;
            }
            let Some(stacking_index) = snapshot.stacking_index else {
                stacking_complete = false;
                continue;
            };
            if seen_indices
                .insert(stacking_index, snapshot.window.xid)
                .is_some()
            {
                stacking_complete = false;
            }
            if target_stacking_index.is_none_or(|target_index| stacking_index <= target_index) {
                continue;
            }
            let Some(rect) = snapshot
                .geometry
                .as_ref()
                .map(|geometry| geometry.frame_rect.unwrap_or(geometry.client_rect).rect)
            else {
                stacking_complete = false;
                continue;
            };
            rectangles.push((stacking_index, snapshot.window.xid, rect));
        }
        rectangles.sort_unstable_by_key(|(stacking_index, xid, _)| (*stacking_index, *xid));
        if rectangles.len() > MAX_ELEMENT_CLICK_OCCLUDERS {
            rectangles.truncate(MAX_ELEMENT_CLICK_OCCLUDERS);
            stacking_complete = false;
        }
        let rectangles_above = rectangles.into_iter().map(|(_, _, rect)| rect).collect();
        self.stacking_epoch = self
            .stacking_epoch
            .checked_add(1)
            .ok_or(ControlPlaneError::Internal)?;

        Ok(WindowOcclusionSnapshot {
            target_window: target.window,
            model_revision,
            target_client_bounds,
            stacking_epoch: self.stacking_epoch,
            rectangles_above,
            stacking_complete,
        })
    }

    fn observe_raw(
        &mut self,
        input: &WindowSnapshotInput,
        now: MonotonicMillis,
    ) -> Result<(), ObservationAdapterError> {
        validate_raw_input(input)?;
        let (_, current) = self
            .model
            .snapshot_all(now)
            .map_err(|_| ObservationAdapterError::Model)?;
        let previous = current
            .iter()
            .find(|snapshot| snapshot.window.xid == input.window)
            .cloned();
        let previous_active = current
            .iter()
            .find(|snapshot| snapshot.state.active)
            .map(|snapshot| snapshot.window.clone());
        let previous_focused = current
            .iter()
            .find(|snapshot| snapshot.state.focused)
            .map(|snapshot| snapshot.window.clone());
        let mut references = current
            .iter()
            .map(|snapshot| (snapshot.window.xid, snapshot.window.clone()))
            .collect::<BTreeMap<_, _>>();
        let reference = if let Some(reference) = references.get(&input.window) {
            reference.clone()
        } else {
            let birth = self
                .model
                .next_birth_serial()
                .map_err(|_| ObservationAdapterError::Model)?;
            let reference = WindowRef {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                xid: input.window,
                observed_generation: birth,
                identity_hash: build_identity_hash(
                    input,
                    self.desktop_id,
                    self.desktop_generation,
                    birth,
                )?,
            };
            references.insert(input.window, reference.clone());
            reference
        };
        let stacking_index = previous
            .as_ref()
            .and_then(|snapshot| snapshot.stacking_index);
        let mut snapshot = normalize_snapshot(
            input,
            reference,
            self.model.revision(),
            stacking_index,
            &references,
        )?;
        let process_universe_changed = previous.as_ref().is_none_or(|previous| {
            previous.window != snapshot.window
                || previous.process.reported_pid != snapshot.process.reported_pid
        });
        if !process_universe_changed && let Some(previous) = &previous {
            snapshot.process = previous.process.clone();
        }
        snapshot.has_accessibility_application = previous
            .as_ref()
            .is_some_and(|snapshot| snapshot.has_accessibility_application);
        let change = self
            .model
            .observe(snapshot, now)
            .map_err(|_| ObservationAdapterError::Model)?;
        let committed = match change {
            WindowModelChange::Created(snapshot) | WindowModelChange::Updated(snapshot) => snapshot,
        };
        if process_universe_changed {
            self.advance_correlation_epoch()?;
            self.process_correlation_available = false;
        }
        self.events.emit_committed_observation(
            WindowEventScope {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                model_revision: self.model.revision(),
            },
            previous_active,
            previous_focused,
            previous.as_ref(),
            &committed,
        );
        Ok(())
    }

    fn remove_xid(
        &mut self,
        xid: u32,
        now: MonotonicMillis,
    ) -> Result<(), ObservationAdapterError> {
        let (_, current) = self
            .model
            .snapshot_all(now)
            .map_err(|_| ObservationAdapterError::Model)?;
        let Some(previous) = current
            .iter()
            .find(|snapshot| snapshot.window.xid == xid)
            .cloned()
        else {
            return Ok(());
        };
        let previous_active = current
            .iter()
            .find(|snapshot| snapshot.state.active)
            .map(|snapshot| snapshot.window.clone());
        let previous_focused = current
            .iter()
            .find(|snapshot| snapshot.state.focused)
            .map(|snapshot| snapshot.window.clone());
        self.model
            .destroy(&previous.window, now)
            .map_err(|_| ObservationAdapterError::Model)?;
        self.advance_correlation_epoch()?;
        self.process_correlation_available = false;
        self.events.emit_committed_removal(
            WindowEventScope {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                model_revision: self.model.revision(),
            },
            previous_active,
            previous_focused,
            &previous,
        );
        Ok(())
    }

    fn list(
        &mut self,
        principal: &str,
        request: &WindowListRequest,
        now: MonotonicMillis,
    ) -> Result<WindowListPage, ControlPlaneError> {
        self.authorize_request(
            request.desktop_id,
            request.desktop_generation,
            request.validate().is_ok(),
        )?;
        let continuation = match request.cursor.as_ref() {
            Some(cursor) => {
                let descriptor = self
                    .tokens
                    .resolve_cursor(
                        cursor,
                        principal,
                        request.desktop_id,
                        request.desktop_generation,
                        request.order,
                        CursorQueryBinding::List,
                        None,
                        now,
                    )
                    .map_err(|_| ControlPlaneError::NotFound)?;
                if descriptor.snapshot_revision != self.model.revision() {
                    return Err(ControlPlaneError::NotFound);
                }
                Some(descriptor)
            }
            None => None,
        };
        let (revision, records) = self
            .model
            .snapshot_query_records(now)
            .map_err(map_model_error)?;
        let view =
            WindowQueryView::new(self.desktop_id, self.desktop_generation, revision, &records)
                .map_err(map_query_error)?;
        let projection = view
            .list(request.order, request.limit, continuation.as_ref())
            .map_err(map_query_error)?;
        self.page_to_list(principal, projection, now)
    }

    fn query(
        &mut self,
        principal: &str,
        request: &WindowQueryRequest,
        now: MonotonicMillis,
    ) -> Result<WindowQueryPage, ControlPlaneError> {
        self.authorize_request(
            request.desktop_id,
            request.desktop_generation,
            request.validate().is_ok(),
        )?;
        let binding =
            selector_binding(&request.selector).map_err(|_| ControlPlaneError::Internal)?;
        let continuation = match request.cursor.as_ref() {
            Some(cursor) => {
                let descriptor = self
                    .tokens
                    .resolve_cursor(
                        cursor,
                        principal,
                        request.desktop_id,
                        request.desktop_generation,
                        request.order,
                        binding,
                        selector_requires_managed_process(&request.selector)
                            .then_some(self.correlation_epoch),
                        now,
                    )
                    .map_err(|_| ControlPlaneError::NotFound)?;
                if descriptor.snapshot_revision != self.model.revision() {
                    return Err(ControlPlaneError::NotFound);
                }
                Some(descriptor)
            }
            None => None,
        };
        let (revision, records) = self
            .model
            .snapshot_query_records(now)
            .map_err(map_model_error)?;
        let view =
            WindowQueryView::new(self.desktop_id, self.desktop_generation, revision, &records)
                .map_err(map_query_error)?;
        let projection = view
            .query(
                &request.selector,
                request.order,
                request.limit,
                continuation.as_ref(),
            )
            .map_err(map_query_error)?;
        self.page_to_query(
            principal,
            projection,
            selector_requires_managed_process(&request.selector),
            now,
        )
    }

    fn snapshot(
        &mut self,
        principal: &str,
        request: &WindowSnapshotRequest,
        now: MonotonicMillis,
    ) -> Result<WindowSnapshotResult, ControlPlaneError> {
        self.authorize_request(
            request.desktop_id,
            request.desktop_generation,
            request.validate().is_ok(),
        )?;
        let reference = match &request.target {
            WindowSnapshotTarget::Reference { window } => window.clone(),
            WindowSnapshotTarget::Token { token } => self
                .tokens
                .resolve_reference(
                    token,
                    principal,
                    request.desktop_id,
                    request.desktop_generation,
                    now,
                )
                .map_err(|_| ControlPlaneError::NotFound)?,
        };
        let resolved = self
            .model
            .resolve_exact(&reference, now)
            .map_err(map_model_error)?;
        let entry = self.entry(principal, resolved.snapshot, now)?;
        Ok(WindowSnapshotResult {
            snapshot_revision: resolved.revision,
            window: entry,
        })
    }

    fn resolve(
        &mut self,
        principal: &str,
        request: &WindowResolveRequest,
        now: MonotonicMillis,
    ) -> Result<WindowResolveResult, ControlPlaneError> {
        self.authorize_request(
            request.desktop_id,
            request.desktop_generation,
            request.validate().is_ok(),
        )?;
        let (revision, records) = self
            .model
            .snapshot_query_records(now)
            .map_err(map_model_error)?;
        let view =
            WindowQueryView::new(self.desktop_id, self.desktop_generation, revision, &records)
                .map_err(map_query_error)?;
        let projection = view
            .resolve(&request.selector, request.order, request.match_policy)
            .map_err(map_query_error)?;
        self.resolve_projection(principal, projection, now)
    }

    fn evaluate_wait(
        &mut self,
        principal: &str,
        request: &WindowWaitRequest,
        now: MonotonicMillis,
        terminal: Option<WindowWaitStatus>,
    ) -> Result<Option<WindowWaitResult>, ControlPlaneError> {
        self.authorize_request(
            request.desktop_id,
            request.desktop_generation,
            request.validate().is_ok(),
        )?;
        let (revision, records) = self
            .model
            .snapshot_query_records(now)
            .map_err(map_model_error)?;
        let view =
            WindowQueryView::new(self.desktop_id, self.desktop_generation, revision, &records)
                .map_err(map_query_error)?;
        let evaluation = view
            .evaluate_wait(&request.target, &request.predicate)
            .map_err(map_query_error)?;
        if let Some(status) = terminal {
            let snapshots = evaluation
                .satisfying_windows
                .into_iter()
                .take(usize::from(MAX_WINDOW_PAGE_LIMIT))
                .cloned()
                .collect::<Vec<_>>();
            let matched_count = evaluation.satisfying_count;
            let windows = self.entries(principal, snapshots, now)?;
            return Ok(Some(WindowWaitResult {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                status,
                evaluated_revision: revision,
                predicate_satisfied: false,
                matched_count,
                windows,
            }));
        }
        let after_boundary_met = request.after_revision.is_none_or(|after| revision > after);
        let vanished = matches!(request.target, WindowWaitTarget::Reference { .. })
            && !matches!(request.predicate, WindowWaitPredicate::Closed)
            && evaluation.selected_count == 0;
        if vanished {
            return Ok(Some(WindowWaitResult {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                status: WindowWaitStatus::TargetVanished,
                evaluated_revision: revision,
                predicate_satisfied: false,
                matched_count: 0,
                windows: Vec::new(),
            }));
        }
        if !evaluation.predicate_satisfied || !after_boundary_met {
            return Ok(None);
        }
        let snapshots = evaluation
            .satisfying_windows
            .into_iter()
            .take(usize::from(MAX_WINDOW_PAGE_LIMIT))
            .cloned()
            .collect::<Vec<_>>();
        let matched_count = evaluation.satisfying_count;
        let windows = self.entries(principal, snapshots, now)?;
        Ok(Some(WindowWaitResult {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            status: WindowWaitStatus::Matched,
            evaluated_revision: revision,
            predicate_satisfied: true,
            matched_count,
            windows,
        }))
    }

    fn timed_out_wait(
        &mut self,
        principal: &str,
        request: &WindowWaitRequest,
        now: MonotonicMillis,
    ) -> Result<WindowWaitResult, ControlPlaneError> {
        let (revision, records) = self
            .model
            .snapshot_query_records(now)
            .map_err(map_model_error)?;
        let view =
            WindowQueryView::new(self.desktop_id, self.desktop_generation, revision, &records)
                .map_err(map_query_error)?;
        let evaluation = view
            .evaluate_wait(&request.target, &request.predicate)
            .map_err(map_query_error)?;
        let snapshots = evaluation
            .satisfying_windows
            .into_iter()
            .take(usize::from(MAX_WINDOW_PAGE_LIMIT))
            .cloned()
            .collect::<Vec<_>>();
        let windows = self.entries(principal, snapshots, now)?;
        Ok(WindowWaitResult {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            status: WindowWaitStatus::TimedOut,
            evaluated_revision: revision,
            predicate_satisfied: false,
            matched_count: evaluation.satisfying_count,
            windows,
        })
    }

    fn authorize_request(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        valid: bool,
    ) -> Result<(), ControlPlaneError> {
        if !valid {
            return Err(ControlPlaneError::InvalidRequest);
        }
        if desktop_id != self.desktop_id || desktop_generation != self.desktop_generation {
            return Err(ControlPlaneError::PermissionDenied);
        }
        Ok(())
    }

    fn page_to_list(
        &mut self,
        principal: &str,
        projection: WindowPageProjection,
        now: MonotonicMillis,
    ) -> Result<WindowListPage, ControlPlaneError> {
        let windows = self.entries(principal, projection.windows, now)?;
        let next_cursor = projection
            .continuation
            .map(|descriptor| self.tokens.mint_cursor(principal, descriptor, None, now))
            .transpose()
            .map_err(|_| ControlPlaneError::Internal)?;
        Ok(WindowListPage {
            desktop_id: projection.desktop_id,
            desktop_generation: projection.desktop_generation,
            snapshot_revision: projection.snapshot_revision,
            windows,
            next_cursor,
        })
    }

    fn page_to_query(
        &mut self,
        principal: &str,
        projection: WindowPageProjection,
        requires_managed_process: bool,
        now: MonotonicMillis,
    ) -> Result<WindowQueryPage, ControlPlaneError> {
        let windows = self.entries(principal, projection.windows, now)?;
        let next_cursor = projection
            .continuation
            .map(|descriptor| {
                self.tokens.mint_cursor(
                    principal,
                    descriptor,
                    requires_managed_process.then_some(self.correlation_epoch),
                    now,
                )
            })
            .transpose()
            .map_err(|_| ControlPlaneError::Internal)?;
        Ok(WindowQueryPage {
            desktop_id: projection.desktop_id,
            desktop_generation: projection.desktop_generation,
            snapshot_revision: projection.snapshot_revision,
            windows,
            next_cursor,
        })
    }

    fn resolve_projection(
        &mut self,
        principal: &str,
        projection: WindowResolveProjection,
        now: MonotonicMillis,
    ) -> Result<WindowResolveResult, ControlPlaneError> {
        let window = self.entry(principal, projection.window, now)?;
        Ok(WindowResolveResult {
            desktop_id: projection.desktop_id,
            desktop_generation: projection.desktop_generation,
            snapshot_revision: projection.snapshot_revision,
            window,
        })
    }

    fn entries(
        &mut self,
        principal: &str,
        snapshots: Vec<WindowSnapshot>,
        now: MonotonicMillis,
    ) -> Result<Vec<WindowSnapshotEntry>, ControlPlaneError> {
        snapshots
            .into_iter()
            .map(|snapshot| self.entry(principal, snapshot, now))
            .collect()
    }

    fn entry(
        &mut self,
        principal: &str,
        snapshot: WindowSnapshot,
        now: MonotonicMillis,
    ) -> Result<WindowSnapshotEntry, ControlPlaneError> {
        let reference_token = self
            .tokens
            .mint_reference(principal, snapshot.window.clone(), now)
            .map_err(|_| ControlPlaneError::Internal)?;
        Ok(WindowSnapshotEntry {
            snapshot,
            reference_token,
        })
    }
}

fn validate_inventory(
    inventory: &RootInventory,
    inputs: &[WindowSnapshotInput],
) -> Result<(), ObservationAdapterError> {
    if inventory.windows.len() > MAX_ROOT_WINDOWS
        || inventory.windows.len() != inputs.len()
        || inventory
            .warnings
            .iter()
            .any(|warning| matches!(warning, InventoryWarning::Truncated))
    {
        return Err(ObservationAdapterError::InvalidRawObservation);
    }
    let mut windows = BTreeMap::new();
    for (index, xid) in inventory.windows.iter().copied().enumerate() {
        if xid == 0 || windows.insert(xid, index).is_some() || inputs[index].window != xid {
            return Err(ObservationAdapterError::InvalidRawObservation);
        }
        validate_raw_input(&inputs[index])?;
    }
    Ok(())
}

fn map_model_error(error: WindowModelError) -> ControlPlaneError {
    match error {
        WindowModelError::NotFound
        | WindowModelError::StaleReference
        | WindowModelError::DestroyedReference
        | WindowModelError::AlreadyDestroyed => ControlPlaneError::NotFound,
        WindowModelError::WrongDesktopLifetime => ControlPlaneError::PermissionDenied,
        WindowModelError::LiveCapacityExhausted => ControlPlaneError::ResourceExhausted,
        WindowModelError::InvalidReference(_) | WindowModelError::InvalidSnapshot(_) => {
            ControlPlaneError::InvalidRequest
        }
        _ => ControlPlaneError::Internal,
    }
}

fn map_query_error(error: WindowQueryError) -> ControlPlaneError {
    match error {
        WindowQueryError::NoMatch => ControlPlaneError::NotFound,
        WindowQueryError::Ambiguous { .. } => ControlPlaneError::LeaseConflict,
        WindowQueryError::InvalidPageLimit
        | WindowQueryError::InvalidReference(_)
        | WindowQueryError::InvalidSelector(_)
        | WindowQueryError::IncompatibleWaitTarget => ControlPlaneError::InvalidRequest,
        WindowQueryError::ContinuationMismatch | WindowQueryError::ContinuationOutOfRange => {
            ControlPlaneError::NotFound
        }
        _ => ControlPlaneError::Internal,
    }
}

/// Bounded daemon observation actor settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationServiceSettings {
    request_capacity: usize,
    max_waiters: usize,
    token_capacity: usize,
    cursor_ttl_ms: u64,
    reference_ttl_ms: u64,
    raw_request_timeout: Duration,
    startup_timeout: Duration,
    idle_poll_interval: Duration,
}

impl Default for ObservationServiceSettings {
    fn default() -> Self {
        Self {
            request_capacity: 128,
            max_waiters: 128,
            token_capacity: 8_192,
            cursor_ttl_ms: 60_000,
            reference_ttl_ms: 15 * 60_000,
            raw_request_timeout: Duration::from_secs(2),
            startup_timeout: Duration::from_secs(10),
            idle_poll_interval: Duration::from_millis(5),
        }
    }
}

impl ObservationServiceSettings {
    /// Creates checked bounded actor, waiter, token, and raw-X11 deadlines.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_capacity: usize,
        max_waiters: usize,
        token_capacity: usize,
        cursor_ttl_ms: u64,
        reference_ttl_ms: u64,
        raw_request_timeout: Duration,
        startup_timeout: Duration,
        idle_poll_interval: Duration,
    ) -> Result<Self, ObservationCompositionError> {
        Self {
            request_capacity,
            max_waiters,
            token_capacity,
            cursor_ttl_ms,
            reference_ttl_ms,
            raw_request_timeout,
            startup_timeout,
            idle_poll_interval,
        }
        .validate()
    }

    fn validate(self) -> Result<Self, ObservationCompositionError> {
        if self.request_capacity == 0
            || self.max_waiters == 0
            || self.token_capacity < usize::from(MAX_WINDOW_PAGE_LIMIT)
            || self.cursor_ttl_ms == 0
            || self.reference_ttl_ms == 0
            || self.raw_request_timeout.is_zero()
            || self.startup_timeout.is_zero()
            || self.idle_poll_interval.is_zero()
        {
            return Err(ObservationCompositionError::InvalidSettings);
        }
        Ok(self)
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self {
            request_capacity: 8,
            max_waiters: 2,
            token_capacity: usize::from(MAX_WINDOW_PAGE_LIMIT),
            cursor_ttl_ms: 20,
            reference_ttl_ms: 20,
            raw_request_timeout: Duration::from_millis(100),
            startup_timeout: Duration::from_secs(1),
            idle_poll_interval: Duration::from_millis(1),
        }
    }
}

/// Stable startup failures for the daemon observation composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationCompositionError {
    /// A configured capacity or deadline was zero or internally inconsistent.
    InvalidSettings,
    /// The raw X11 observation actor could not start.
    RawActorUnavailable,
    /// Initial authoritative reconciliation failed or exceeded its deadline.
    InitialReconcileFailed,
    /// The daemon model actor thread could not be created.
    ThreadSpawnFailed,
}

impl fmt::Display for ObservationCompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSettings => "observation service settings are invalid",
            Self::RawActorUnavailable => "raw observation actor is unavailable",
            Self::InitialReconcileFailed => "initial window reconciliation failed",
            Self::ThreadSpawnFailed => "observation model actor could not start",
        })
    }
}

impl Error for ObservationCompositionError {}

/// Terminal result observed by the daemon-owned model actor join capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationServiceExit {
    /// Independent orderly shutdown completed.
    Stopped,
    /// Raw observation or model invariants were terminally poisoned.
    Poisoned,
    /// A panic crossed the actor boundary.
    Panicked,
}

/// Content-free liveness state for synchronous capability projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationServiceState {
    Starting,
    Healthy,
    Stopped,
    Poisoned,
    Panicked,
}

/// Object-safe server adapter backed by the daemon's bounded model actor.
pub struct DaemonObservationService {
    requests: SyncSender<ModelRequest>,
    control: Arc<ModelActorControl>,
    desktop_generation: DesktopGeneration,
    pid_correlator: Arc<dyn PidCorrelator>,
    process_refresh: Arc<ProcessCorrelationRefreshCoordinator>,
    process_lifecycle_authority: Arc<ProcessLifecycleAuthority>,
    wait_terminal_settlement_timeout: Duration,
    event_metrics: Arc<WindowEventDeliveryMetrics>,
    #[cfg(test)]
    unavailable_refresh_hook: Mutex<Option<Arc<UnavailableRefreshHook>>>,
}

struct ProcessCorrelationRefreshState {
    in_flight: bool,
    completion_sequence: u64,
    completion_change_sequence: u64,
    last_result: Result<ProcessCorrelationFence, ControlPlaneError>,
    refreshed_change_sequence: Option<u64>,
}

struct ProcessCorrelationRefreshCoordinator {
    state: Mutex<ProcessCorrelationRefreshState>,
    completed: Notify,
    model_changed: Arc<ProcessCorrelationModelChanges>,
    #[cfg(test)]
    cache_hit_hook: Mutex<Option<Arc<ProcessRefreshCacheHitHook>>>,
}

#[cfg(test)]
struct ProcessRefreshCacheHitHook {
    observed: Notify,
    released: AtomicBool,
    resume: Notify,
}

#[cfg(test)]
impl ProcessRefreshCacheHitHook {
    fn new() -> Self {
        Self {
            observed: Notify::new(),
            released: AtomicBool::new(false),
            resume: Notify::new(),
        }
    }

    async fn pause(&self) {
        self.observed.notify_waiters();
        loop {
            let notified = self.resume.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.resume.notify_waiters();
    }
}

#[cfg(test)]
struct UnavailableRefreshHook {
    observed: Notify,
    released: AtomicBool,
    resume: Notify,
}

#[cfg(test)]
impl UnavailableRefreshHook {
    fn new() -> Self {
        Self {
            observed: Notify::new(),
            released: AtomicBool::new(false),
            resume: Notify::new(),
        }
    }

    async fn pause(&self) {
        self.observed.notify_waiters();
        loop {
            let notified = self.resume.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.resume.notify_waiters();
    }
}

struct ProcessCorrelationModelChanges {
    sequence: AtomicU64,
    notify: Notify,
}

impl ProcessCorrelationModelChanges {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    fn publish(&self) -> u64 {
        let previous = self.sequence.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
        previous.wrapping_add(1)
    }

    fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }
}

impl ProcessCorrelationRefreshCoordinator {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProcessCorrelationRefreshState {
                in_flight: false,
                completion_sequence: 0,
                completion_change_sequence: 0,
                last_result: Err(ControlPlaneError::CapabilityUnavailable),
                refreshed_change_sequence: None,
            }),
            completed: Notify::new(),
            model_changed: Arc::new(ProcessCorrelationModelChanges::new()),
            #[cfg(test)]
            cache_hit_hook: Mutex::new(None),
        }
    }

    fn complete(
        &self,
        result: Result<ProcessCorrelationFence, ControlPlaneError>,
        refreshed_change_sequence: u64,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.in_flight = false;
            state.completion_sequence = state.completion_sequence.wrapping_add(1);
            state.completion_change_sequence = refreshed_change_sequence;
            state.last_result = result;
            state.refreshed_change_sequence = result.is_ok().then_some(refreshed_change_sequence);
        }
        self.completed.notify_waiters();
        result
    }
}

struct ProcessCorrelationRefreshLeader<'a> {
    coordinator: &'a ProcessCorrelationRefreshCoordinator,
    refreshed_change_sequence: u64,
    finished: bool,
}

impl ProcessCorrelationRefreshLeader<'_> {
    fn finish(
        mut self,
        result: Result<ProcessCorrelationFence, ControlPlaneError>,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        self.finished = true;
        self.coordinator
            .complete(result, self.refreshed_change_sequence)
    }
}

impl Drop for ProcessCorrelationRefreshLeader<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.coordinator.complete(
                Err(ControlPlaneError::CapabilityUnavailable),
                self.refreshed_change_sequence,
            );
        }
    }
}

impl fmt::Debug for DaemonObservationService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DaemonObservationService { .. }")
    }
}

/// Independent, coalescing shutdown capability for observation composition.
#[derive(Clone)]
pub struct ObservationServiceShutdown {
    control: Arc<ModelActorControl>,
}

impl ObservationServiceShutdown {
    /// Requests shutdown without entering either bounded ordinary request lane.
    pub fn request(&self) {
        self.control.request_shutdown();
    }
}

/// Owned join capability for the daemon observation model actor.
pub struct ObservationServiceJoin {
    thread: Option<JoinHandle<ObservationServiceExit>>,
    shutdown: ObservationServiceShutdown,
}

impl ObservationServiceJoin {
    /// Joins the model actor and returns its observed terminal state.
    pub fn join(mut self) -> ObservationServiceExit {
        let Some(thread) = self.thread.take() else {
            return ObservationServiceExit::Stopped;
        };
        thread.join().unwrap_or(ObservationServiceExit::Panicked)
    }
}

impl Drop for ObservationServiceJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.shutdown.request();
        let _ = thread.join();
    }
}

/// Starts the raw X11 actor and composes it with the daemon model actor.
///
/// This deliberately does not alter readiness, capabilities, or HTTP routing;
/// callers wire the returned service only after successful reconciliation.
#[allow(clippy::type_complexity)]
#[allow(dead_code)] // Retained as the explicit no-broker, no-event compatibility seam.
pub fn spawn_live_observation_service(
    display: &str,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    let (raw_handle, raw_events, raw_join) = spawn_observation_actor(display)
        .map_err(|_| ObservationCompositionError::RawActorUnavailable)?;
    compose_observation_service(
        raw_handle,
        raw_events,
        raw_join,
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
    )
}

/// Starts live observation with advisory processd PID correlation enabled.
///
/// Broker evidence enriches public observations only after ordinary
/// authorization and model resolution. Broker failure never makes window
/// observation unavailable and correlation never authorizes process control.
#[allow(clippy::type_complexity)]
#[allow(dead_code)] // Explicit broker opt-in is wired by the final daemon composition pass.
pub fn spawn_live_observation_service_with_broker(
    display: &str,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    broker: BrokerClient,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    let (raw_handle, raw_events, raw_join) = spawn_observation_actor(display)
        .map_err(|_| ObservationCompositionError::RawActorUnavailable)?;
    compose_observation_service_with_broker(
        raw_handle,
        raw_events,
        raw_join,
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
        broker,
    )
}

/// Starts live observation with PID correlation and an explicit normalized
/// event sink. The sink remains nonblocking and sequence-free; daemon lifecycle
/// wiring supplies the coordinator ingress in a later composition pass.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
#[allow(dead_code)]
pub(crate) fn spawn_live_observation_service_with_broker_and_event_sink(
    display: &str,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    broker: BrokerClient,
    event_sink: Arc<dyn WindowEventSink>,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    let (raw_handle, raw_events, raw_join) = spawn_observation_actor(display)
        .map_err(|_| ObservationCompositionError::RawActorUnavailable)?;
    spawn_model_actor_with_components(
        Box::new(LiveRawBackend::new(raw_handle, raw_events, raw_join)),
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
        Arc::new(BrokerPidCorrelator(broker)),
        event_sink,
    )
}

/// Composes already-started raw X11 actor capabilities into daemon state.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
#[allow(dead_code)] // Supports injected raw actors without enabling optional integrations.
pub fn compose_observation_service(
    raw_handle: ObservationActorHandle,
    raw_events: ObservationActorEventReceiver,
    raw_join: ObservationActorJoin,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    spawn_model_actor(
        Box::new(LiveRawBackend::new(raw_handle, raw_events, raw_join)),
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
    )
}

/// Composes started X11 observation actors with advisory processd correlation.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
#[allow(dead_code)] // Kept for deterministic composition tests and later daemon wiring.
pub fn compose_observation_service_with_broker(
    raw_handle: ObservationActorHandle,
    raw_events: ObservationActorEventReceiver,
    raw_join: ObservationActorJoin,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    broker: BrokerClient,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    spawn_model_actor_with_correlator(
        Box::new(LiveRawBackend::new(raw_handle, raw_events, raw_join)),
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
        Arc::new(BrokerPidCorrelator(broker)),
    )
}

impl DaemonObservationService {
    /// Returns the model actor state without submitting work or touching X11.
    #[must_use]
    pub(crate) fn health(&self) -> ObservationServiceState {
        self.control.health()
    }

    pub(crate) fn enable_process_lifecycle_authority(&self) {
        self.process_lifecycle_authority.enable();
    }

    pub(crate) fn disable_process_lifecycle_authority(&self) {
        self.process_lifecycle_authority.disable();
    }

    fn process_fence_is_current(&self, fence: ProcessCorrelationFence) -> bool {
        self.process_lifecycle_authority
            .accepts(fence.lifecycle_authority)
    }

    /// Returns content-free counts of events dropped before coordinator ingress.
    #[allow(dead_code)]
    pub(crate) fn window_event_drop_stats(&self) -> WindowEventDropStats {
        self.event_metrics.snapshot()
    }

    /// Atomically replaces the exact live windows correlated to accessible
    /// applications and returns the committed window-model revision.
    ///
    /// An empty set clears correlation after an AT-SPI gap or reconnect. Every
    /// non-empty entry must still resolve to the exact observed window birth;
    /// an XID alone can never carry correlation across reuse.
    #[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
    pub(crate) async fn replace_accessibility_correlations(
        &self,
        expected_revision: WindowModelRevision,
        windows: Vec<WindowRef>,
    ) -> Result<WindowModelRevision, ControlPlaneError> {
        if windows.len() > MAX_ROOT_WINDOWS
            || windows
                .iter()
                .any(|window| window.validate_shape().is_err())
        {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let (response, receiver) = oneshot::channel();
        self.submit(ModelRequest::ReplaceAccessibilityCorrelations {
            expected_revision,
            windows,
            response,
        })?;
        receiver
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
    }

    /// Returns one complete, bounded, revision-fenced view for accessibility
    /// correlation. The model owner rejects an oversized live set rather than
    /// returning a misleading partial candidate universe.
    #[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
    pub(crate) async fn accessibility_correlation_snapshot(
        &self,
    ) -> Result<ObservationCorrelationSnapshot, ControlPlaneError> {
        let fence = self.refresh_for_operation(false).await?;
        self.pause_after_unavailable_refresh(fence).await;
        let observed_sequence = self.process_refresh.model_changed.sequence();
        let (response, receiver) = oneshot::channel();
        self.submit(ModelRequest::AccessibilityCorrelationSnapshot { response })?;
        let mut snapshot = receiver
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
        if snapshot.windows.iter().any(|window| {
            window.window.desktop_generation != self.desktop_generation
                || window.model_revision != snapshot.revision
                || window.validate().is_err()
        }) {
            return Err(ControlPlaneError::Internal);
        }
        if snapshot
            .windows
            .iter()
            .any(|window| window.validate().is_err())
        {
            return Err(ControlPlaneError::Internal);
        }
        if fence.is_none()
            || self.process_refresh.model_changed.sequence() != observed_sequence
            || fence.is_some_and(|fence| {
                fence.revision != snapshot.revision || !self.process_fence_is_current(fence)
            })
        {
            for window in &mut snapshot.windows {
                window.process = baseline_process_correlation(window.process.reported_pid);
            }
        }
        Ok(snapshot)
    }

    /// Reads one complete candidate universe after pending raw X11 events have
    /// been drained, for an input actor's queue-head correlation recheck.
    pub(crate) fn accessibility_correlation_snapshot_blocking(
        &self,
        timeout: Duration,
    ) -> Result<ObservationCorrelationSnapshot, ControlPlaneError> {
        if timeout.is_zero() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.submit(ModelRequest::AccessibilityCorrelationSnapshotBlocking { response })?;
        let mut snapshot = receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected => {
                    ControlPlaneError::CapabilityUnavailable
                }
            })??;
        for window in &mut snapshot.windows {
            window.process = baseline_process_correlation(window.process.reported_pid);
        }
        Ok(snapshot)
    }

    fn submit(&self, request: ModelRequest) -> Result<(), ControlPlaneError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => ControlPlaneError::ResourceExhausted,
                TrySendError::Disconnected(_) => ControlPlaneError::CapabilityUnavailable,
            })?;
        self.control.notify();
        Ok(())
    }

    async fn refresh_process_correlations(
        &self,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        enum RefreshAction {
            Cached(Result<ProcessCorrelationFence, ControlPlaneError>),
            Lead,
            Follow(u64),
        }

        loop {
            let change_sequence = self.process_refresh.model_changed.sequence();
            let notified = self.process_refresh.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let action = {
                let mut state = lock_unpoisoned(&self.process_refresh.state);
                if state.refreshed_change_sequence == Some(change_sequence) {
                    RefreshAction::Cached(state.last_result)
                } else if !state.in_flight {
                    state.in_flight = true;
                    RefreshAction::Lead
                } else {
                    RefreshAction::Follow(state.completion_sequence)
                }
            };
            match action {
                RefreshAction::Cached(result) => {
                    #[cfg(test)]
                    {
                        let hook = lock_unpoisoned(&self.process_refresh.cache_hit_hook).clone();
                        if let Some(hook) = hook {
                            hook.pause().await;
                        }
                    }
                    if self.process_refresh.model_changed.sequence() == change_sequence
                        && result.is_ok_and(|fence| self.process_fence_is_current(fence))
                    {
                        return result;
                    }
                }
                RefreshAction::Lead => {
                    let mut leader = ProcessCorrelationRefreshLeader {
                        coordinator: self.process_refresh.as_ref(),
                        refreshed_change_sequence: change_sequence,
                        finished: false,
                    };
                    let result =
                        self.perform_process_correlation_refresh()
                            .await
                            .and_then(|commit| {
                                leader.refreshed_change_sequence = commit.change_sequence;
                                (self.process_refresh.model_changed.sequence()
                                    == commit.change_sequence)
                                    .then_some(commit.fence)
                                    .ok_or(ControlPlaneError::StaleReference {
                                        current_generation: Some(self.desktop_generation),
                                    })
                            });
                    if result.is_err() {
                        leader.refreshed_change_sequence =
                            self.process_refresh.model_changed.sequence();
                    }
                    return leader.finish(result);
                }
                RefreshAction::Follow(observed_sequence) => {
                    notified.await;
                    let state = lock_unpoisoned(&self.process_refresh.state);
                    if state.completion_sequence != observed_sequence {
                        let result = state.last_result;
                        let completion_change_sequence = state.completion_change_sequence;
                        drop(state);
                        if self.process_refresh.model_changed.sequence()
                            == completion_change_sequence
                        {
                            return result;
                        }
                    }
                }
            }
        }
    }

    async fn perform_process_correlation_refresh(
        &self,
    ) -> Result<ProcessCorrelationCommit, ControlPlaneError> {
        let lifecycle_authority = self.process_lifecycle_authority.capture();
        if !lifecycle_authority.available() {
            self.invalidate_process_correlations_internal().await?;
            return Err(ControlPlaneError::CapabilityUnavailable);
        }
        let (snapshot_response, snapshot_receiver) = oneshot::channel();
        self.submit(ModelRequest::ProcessCorrelationSnapshot {
            response: snapshot_response,
        })?;
        let snapshot = snapshot_receiver
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
        if snapshot.windows.iter().any(|window| {
            window.window.desktop_generation != self.desktop_generation
                || window.model_revision != snapshot.revision
                || window.validate().is_err()
        }) {
            self.invalidate_process_correlations_internal().await?;
            return Err(ControlPlaneError::CapabilityUnavailable);
        }

        let pids = unique_reported_pids(snapshot.windows.iter());
        let upgrades = match resolve_process_correlation_upgrades(
            self.pid_correlator.as_ref(),
            self.desktop_generation,
            pids,
        )
        .await
        {
            Ok(upgrades) => upgrades,
            Err(_) => {
                self.invalidate_process_correlations_internal().await?;
                return Err(ControlPlaneError::CapabilityUnavailable);
            }
        };
        let assignments = snapshot
            .windows
            .into_iter()
            .map(|window| ProcessCorrelationAssignment {
                reported_pid: window.process.reported_pid,
                upgrade: window
                    .process
                    .reported_pid
                    .and_then(|pid| upgrades.get(&pid).copied()),
                window: window.window,
            })
            .collect();
        let (replace_response, replace_receiver) = oneshot::channel();
        self.submit(ModelRequest::ReplaceProcessCorrelations {
            expected_revision: snapshot.revision,
            expected_correlation_epoch: snapshot.correlation_epoch,
            expected_generation: self.desktop_generation,
            expected_lifecycle_authority: lifecycle_authority,
            assignments,
            response: replace_response,
        })?;
        replace_receiver
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
    }

    async fn invalidate_process_correlations_internal(
        &self,
    ) -> Result<ProcessCorrelationFence, ControlPlaneError> {
        let (response, receiver) = oneshot::channel();
        self.submit(ModelRequest::InvalidateProcessCorrelations { response })?;
        receiver
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
    }

    /// Clears every broker-confirmed process assignment through the model owner.
    ///
    /// The process-event relay can call this on exit, replay gaps, or broker
    /// reconnect before a fresh correlation pass. Invalidation advances the
    /// correlation epoch even when the visible snapshots were already low
    /// confidence, fencing a concurrently captured broker response.
    #[allow(dead_code, reason = "process lifecycle integration seam")]
    pub(crate) async fn invalidate_process_correlations(&self) -> Result<(), ControlPlaneError> {
        self.process_lifecycle_authority.disable();
        self.invalidate_process_correlations_internal().await?;
        Ok(())
    }

    async fn refresh_for_operation(
        &self,
        requires_managed_process: bool,
    ) -> Result<Option<ProcessCorrelationFence>, ControlPlaneError> {
        if !self.process_lifecycle_authority.capture().available() {
            self.invalidate_process_correlations_internal().await?;
            return if requires_managed_process {
                Err(ControlPlaneError::CapabilityUnavailable)
            } else {
                Ok(None)
            };
        }
        for _ in 0..PROCESS_CORRELATION_REFRESH_ATTEMPTS {
            match self.refresh_process_correlations().await {
                Ok(fence) => return Ok(Some(fence)),
                Err(ControlPlaneError::StaleReference { .. }) => continue,
                Err(ControlPlaneError::CapabilityUnavailable) if !requires_managed_process => {
                    self.invalidate_process_correlations_internal().await?;
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
        if requires_managed_process {
            Err(ControlPlaneError::CapabilityUnavailable)
        } else {
            Ok(None)
        }
    }

    async fn pause_after_unavailable_refresh(&self, fence: Option<ProcessCorrelationFence>) {
        #[cfg(test)]
        if fence.is_none() {
            let hook = lock_unpoisoned(&self.unavailable_refresh_hook).clone();
            if let Some(hook) = hook {
                hook.pause().await;
            }
        }
        #[cfg(not(test))]
        let _ = fence;
    }

    async fn list_for_principal(
        &self,
        principal: String,
        request: WindowListRequest,
    ) -> Result<WindowListPage, ControlPlaneError> {
        for attempt in 0..PROCESS_CORRELATION_REFRESH_ATTEMPTS {
            let fence = self.refresh_for_operation(false).await?;
            self.pause_after_unavailable_refresh(fence).await;
            let observed_sequence = self.process_refresh.model_changed.sequence();
            let (response, receiver) = oneshot::channel();
            self.submit(ModelRequest::List {
                principal: principal.clone(),
                request: request.clone(),
                response,
            })?;
            let mut result = receiver
                .await
                .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
            if fence.is_none() {
                strip_process_from_entries(&mut result.windows);
                return Ok(result);
            }
            if self.process_refresh.model_changed.sequence() == observed_sequence
                && fence.is_some_and(|fence| {
                    fence.revision == result.snapshot_revision
                        && self.process_fence_is_current(fence)
                })
            {
                return Ok(result);
            }
            if attempt + 1 == PROCESS_CORRELATION_REFRESH_ATTEMPTS {
                strip_process_from_entries(&mut result.windows);
                return Ok(result);
            }
        }
        Err(ControlPlaneError::CapabilityUnavailable)
    }

    async fn snapshot_for_principal(
        &self,
        principal: String,
        request: WindowSnapshotRequest,
    ) -> Result<WindowSnapshotResult, ControlPlaneError> {
        for attempt in 0..PROCESS_CORRELATION_REFRESH_ATTEMPTS {
            let fence = self.refresh_for_operation(false).await?;
            self.pause_after_unavailable_refresh(fence).await;
            let observed_sequence = self.process_refresh.model_changed.sequence();
            let (response, receiver) = oneshot::channel();
            self.submit(ModelRequest::Snapshot {
                principal: principal.clone(),
                request: request.clone(),
                response,
            })?;
            let mut result = receiver
                .await
                .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
            if fence.is_none() {
                strip_process_from_entry(&mut result.window);
                return Ok(result);
            }
            if self.process_refresh.model_changed.sequence() == observed_sequence
                && fence.is_some_and(|fence| {
                    fence.revision == result.snapshot_revision
                        && self.process_fence_is_current(fence)
                })
            {
                return Ok(result);
            }
            if attempt + 1 == PROCESS_CORRELATION_REFRESH_ATTEMPTS {
                strip_process_from_entry(&mut result.window);
                return Ok(result);
            }
        }
        Err(ControlPlaneError::CapabilityUnavailable)
    }

    async fn query_for_principal(
        &self,
        principal: String,
        request: WindowQueryRequest,
    ) -> Result<WindowQueryPage, ControlPlaneError> {
        let requires_managed_process = selector_requires_managed_process(&request.selector);
        for attempt in 0..PROCESS_CORRELATION_REFRESH_ATTEMPTS {
            let fence = self.refresh_for_operation(requires_managed_process).await?;
            self.pause_after_unavailable_refresh(fence).await;
            let observed_sequence = self.process_refresh.model_changed.sequence();
            let (response, receiver) = oneshot::channel();
            self.submit(ModelRequest::Query {
                principal: principal.clone(),
                request: request.clone(),
                response,
            })?;
            let mut result = receiver
                .await
                .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
            if fence.is_none() {
                strip_process_from_entries(&mut result.windows);
                return Ok(result);
            }
            if self.process_refresh.model_changed.sequence() == observed_sequence
                && fence.is_some_and(|fence| {
                    fence.revision == result.snapshot_revision
                        && self.process_fence_is_current(fence)
                })
            {
                return Ok(result);
            }
            if attempt + 1 == PROCESS_CORRELATION_REFRESH_ATTEMPTS {
                return if requires_managed_process {
                    Err(ControlPlaneError::CapabilityUnavailable)
                } else {
                    strip_process_from_entries(&mut result.windows);
                    Ok(result)
                };
            }
        }
        Err(ControlPlaneError::CapabilityUnavailable)
    }

    async fn resolve_for_principal(
        &self,
        principal: String,
        request: WindowResolveRequest,
    ) -> Result<WindowResolveResult, ControlPlaneError> {
        let requires_managed_process = selector_requires_managed_process(&request.selector);
        for attempt in 0..PROCESS_CORRELATION_REFRESH_ATTEMPTS {
            let fence = self.refresh_for_operation(requires_managed_process).await?;
            self.pause_after_unavailable_refresh(fence).await;
            let observed_sequence = self.process_refresh.model_changed.sequence();
            let (response, receiver) = oneshot::channel();
            self.submit(ModelRequest::Resolve {
                principal: principal.clone(),
                request: request.clone(),
                response,
            })?;
            let mut result = receiver
                .await
                .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
            if fence.is_none() {
                strip_process_from_entry(&mut result.window);
                return Ok(result);
            }
            if self.process_refresh.model_changed.sequence() == observed_sequence
                && fence.is_some_and(|fence| {
                    fence.revision == result.snapshot_revision
                        && self.process_fence_is_current(fence)
                })
            {
                return Ok(result);
            }
            if attempt + 1 == PROCESS_CORRELATION_REFRESH_ATTEMPTS {
                return if requires_managed_process {
                    Err(ControlPlaneError::CapabilityUnavailable)
                } else {
                    strip_process_from_entry(&mut result.window);
                    Ok(result)
                };
            }
        }
        Err(ControlPlaneError::CapabilityUnavailable)
    }

    async fn wait_for_principal(
        &self,
        principal: String,
        request: WindowWaitRequest,
    ) -> Result<WindowWaitResult, ControlPlaneError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(u64::from(request.timeout_ms)))
            .ok_or(ControlPlaneError::InvalidRequest)?;
        let async_deadline = tokio::time::Instant::from_std(deadline);
        let requires_managed_process = wait_requires_managed_process(&request);
        let (response, mut receiver) = oneshot::channel();
        self.submit(ModelRequest::Wait {
            principal: principal.clone(),
            request: request.clone(),
            deadline: Some(deadline),
            response,
        })?;

        let changed = self.process_refresh.model_changed.notify.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let mut latest_fence = match tokio::time::timeout_at(
            async_deadline,
            self.refresh_for_operation(requires_managed_process),
        )
        .await
        {
            Ok(result) => {
                result?.map(|fence| (fence, self.process_refresh.model_changed.sequence()))
            }
            Err(_) => None,
        };
        let mut observed_change_sequence = self.process_refresh.model_changed.sequence();
        loop {
            if Instant::now() >= deadline {
                let settlement_deadline = tokio::time::Instant::now()
                    .checked_add(self.wait_terminal_settlement_timeout)
                    .ok_or(ControlPlaneError::Internal)?;
                let mut result = tokio::time::timeout_at(settlement_deadline, &mut receiver)
                    .await
                    .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
                    .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
                let stable = latest_fence.is_some_and(|(fence, sequence)| {
                    fence.revision == result.evaluated_revision
                        && self.process_fence_is_current(fence)
                        && self.process_refresh.model_changed.sequence() == sequence
                });
                if stable {
                    return Ok(result);
                }
                if requires_managed_process {
                    return Err(ControlPlaneError::CapabilityUnavailable);
                }
                for window in &mut result.windows {
                    window.snapshot.process =
                        baseline_process_correlation(window.snapshot.process.reported_pid);
                }
                return Ok(result);
            }
            if self.process_refresh.model_changed.sequence() != observed_change_sequence {
                observed_change_sequence = self.process_refresh.model_changed.sequence();
                let next_change = self.process_refresh.model_changed.notify.notified();
                changed.set(next_change);
                changed.as_mut().enable();
                let refresh = tokio::time::timeout_at(
                    async_deadline,
                    self.refresh_for_operation(requires_managed_process),
                )
                .await;
                match refresh {
                    Ok(result) => {
                        latest_fence = result?
                            .map(|fence| (fence, self.process_refresh.model_changed.sequence()));
                    }
                    Err(_) => latest_fence = None,
                }
                continue;
            }
            tokio::select! {
                result = &mut receiver => {
                    let mut result =
                        result.map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
                    if latest_fence.is_none() && !requires_managed_process {
                        for window in &mut result.windows {
                            window.snapshot.process =
                                baseline_process_correlation(window.snapshot.process.reported_pid);
                        }
                        return Ok(result);
                    }
                    let stable = latest_fence.is_some_and(|(fence, sequence)| {
                        fence.revision == result.evaluated_revision
                            && self.process_fence_is_current(fence)
                            && self.process_refresh.model_changed.sequence() == sequence
                    });
                    if stable {
                        return Ok(result);
                    }
                    if Instant::now() < deadline {
                        let (response, next_receiver) = oneshot::channel();
                        self.submit(ModelRequest::Wait {
                            principal: principal.clone(),
                            request: request.clone(),
                            deadline: Some(deadline),
                            response,
                        })?;
                        receiver = next_receiver;
                        continue;
                    }
                    if requires_managed_process {
                        return Err(ControlPlaneError::CapabilityUnavailable);
                    }
                    for window in &mut result.windows {
                        window.snapshot.process =
                            baseline_process_correlation(window.snapshot.process.reported_pid);
                    }
                    return Ok(result);
                }
                () = &mut changed => {
                    observed_change_sequence = self.process_refresh.model_changed.sequence();
                    let next_change = self.process_refresh.model_changed.notify.notified();
                    changed.set(next_change);
                    changed.as_mut().enable();
                    let refresh = tokio::time::timeout_at(
                        async_deadline,
                        self.refresh_for_operation(requires_managed_process),
                    )
                    .await;
                    match refresh {
                        Ok(result) => {
                            latest_fence = result?.map(|fence| {
                                (
                                    fence,
                                    self.process_refresh.model_changed.sequence(),
                                )
                            });
                        }
                        Err(_) => latest_fence = None,
                    }
                }
                () = tokio::time::sleep_until(async_deadline) => {
                    let settlement_deadline = tokio::time::Instant::now()
                        .checked_add(self.wait_terminal_settlement_timeout)
                        .ok_or(ControlPlaneError::Internal)?;
                    let mut result = tokio::time::timeout_at(settlement_deadline, &mut receiver)
                        .await
                        .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
                        .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
                    let stable = latest_fence.is_some_and(|(fence, sequence)| {
                        fence.revision == result.evaluated_revision
                            && self.process_fence_is_current(fence)
                            && self.process_refresh.model_changed.sequence() == sequence
                    });
                    if stable {
                        return Ok(result);
                    }
                    if requires_managed_process {
                        return Err(ControlPlaneError::CapabilityUnavailable);
                    }
                    for window in &mut result.windows {
                        window.snapshot.process =
                            baseline_process_correlation(window.snapshot.process.reported_pid);
                    }
                    return Ok(result);
                }
            }
        }
    }

    /// Revalidates one exact live window birth on the model-owner thread.
    ///
    /// This synchronous boundary exists for the dedicated raw window-control
    /// actor's mandatory immediately-before-effect callback. It never exposes
    /// model internals or accepts an XID without the full generation-bound
    /// reference.
    pub(crate) fn revalidate_exact_blocking(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<u32, ControlPlaneError> {
        if timeout.is_zero() || window.validate_shape().is_err() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.submit(ModelRequest::Revalidate { window, response })?;
        receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected => {
                    ControlPlaneError::CapabilityUnavailable
                }
            })?
    }

    /// Returns the latest normalized snapshot for one exact live birth.
    ///
    /// The model actor drains pending raw X11 events before ordinary requests,
    /// so window-control completion uses this boundary to publish model-backed
    /// evidence rather than translating raw XIDs directly into public claims.
    pub(crate) fn snapshot_exact_blocking(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<WindowSnapshot, ControlPlaneError> {
        if timeout.is_zero() || window.validate_shape().is_err() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let lifecycle_authority = self.process_lifecycle_authority.capture();
        let (response, receiver) = mpsc::sync_channel(1);
        self.submit(ModelRequest::InternalSnapshot { window, response })?;
        let mut snapshot = receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected => {
                    ControlPlaneError::CapabilityUnavailable
                }
            })??;
        if !self
            .process_lifecycle_authority
            .accepts(lifecycle_authority)
        {
            snapshot.process = baseline_process_correlation(snapshot.process.reported_pid);
        }
        Ok(snapshot)
    }

    /// Reads fresh bounded stacking evidence for one exact target birth.
    ///
    /// Raw events are drained before this model request. The synchronous seam is
    /// therefore suitable for a control actor's queue-head input precondition.
    #[allow(dead_code, reason = "consumed by the deferred input precondition")]
    pub(crate) fn occlusion_snapshot_exact_blocking(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<WindowOcclusionSnapshot, ControlPlaneError> {
        if timeout.is_zero() || window.validate_shape().is_err() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.submit(ModelRequest::OcclusionSnapshot { window, response })?;
        receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected => {
                    ControlPlaneError::CapabilityUnavailable
                }
            })?
    }
}

type PidCorrelationFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<BrokerPidCorrelation>, PidCorrelationError>> + Send + 'a>,
>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PidCorrelationError;

trait PidCorrelator: Send + Sync + 'static {
    fn correlate<'a>(
        &'a self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> PidCorrelationFuture<'a>;
}

#[derive(Debug)]
#[allow(dead_code)] // Used by the deliberately integration-free composition seam.
struct UnavailablePidCorrelator;

impl PidCorrelator for UnavailablePidCorrelator {
    fn correlate<'a>(&'a self, _: DesktopGeneration, _: Vec<u32>) -> PidCorrelationFuture<'a> {
        Box::pin(async { Err(PidCorrelationError) })
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Constructed only by the explicit broker-enabled composition path.
struct BrokerPidCorrelator(BrokerClient);

impl PidCorrelator for BrokerPidCorrelator {
    fn correlate<'a>(
        &'a self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> PidCorrelationFuture<'a> {
        Box::pin(async move {
            self.0
                .correlate_pids(desktop_generation, pids)
                .await
                .map_err(|_| PidCorrelationError)
        })
    }
}

#[derive(Clone, Copy)]
struct ProcessCorrelationUpgrade {
    process: ProcessRef,
    evidence: WindowProcessEvidence,
}

#[derive(Clone)]
struct ProcessCorrelationAssignment {
    window: WindowRef,
    reported_pid: Option<u32>,
    upgrade: Option<ProcessCorrelationUpgrade>,
}

fn baseline_process_correlation(reported_pid: Option<u32>) -> WindowProcessCorrelation {
    match reported_pid {
        Some(pid) => WindowProcessCorrelation {
            reported_pid: Some(pid),
            managed_process: None,
            confidence: WindowProcessConfidence::Low,
            evidence: vec![WindowProcessEvidence::NetWmPid],
            conflict: false,
        },
        None => WindowProcessCorrelation {
            reported_pid: None,
            managed_process: None,
            confidence: WindowProcessConfidence::None,
            evidence: Vec::new(),
            conflict: false,
        },
    }
}

fn strip_process_from_entry(entry: &mut WindowSnapshotEntry) {
    entry.snapshot.process = baseline_process_correlation(entry.snapshot.process.reported_pid);
}

fn strip_process_from_entries(entries: &mut [WindowSnapshotEntry]) {
    for entry in entries {
        strip_process_from_entry(entry);
    }
}

fn process_correlation_from_assignment(
    assignment: &ProcessCorrelationAssignment,
    desktop_generation: DesktopGeneration,
) -> Result<WindowProcessCorrelation, ControlPlaneError> {
    let Some(upgrade) = assignment.upgrade else {
        return Ok(baseline_process_correlation(assignment.reported_pid));
    };
    let Some(reported_pid) = assignment.reported_pid else {
        return Err(ControlPlaneError::InvalidRequest);
    };
    if upgrade.process.validate().is_err()
        || upgrade.process.desktop_generation != desktop_generation
        || (upgrade.evidence == WindowProcessEvidence::ProcStartTime
            && upgrade.process.pid != reported_pid)
        || !matches!(
            upgrade.evidence,
            WindowProcessEvidence::ProcStartTime | WindowProcessEvidence::ProcessGroup
        )
    {
        return Err(ControlPlaneError::InvalidRequest);
    }
    Ok(WindowProcessCorrelation {
        reported_pid: Some(reported_pid),
        managed_process: Some(upgrade.process),
        confidence: WindowProcessConfidence::High,
        evidence: vec![WindowProcessEvidence::NetWmPid, upgrade.evidence],
        conflict: false,
    })
}

#[cfg(test)]
async fn enrich_entries(
    correlator: &dyn PidCorrelator,
    desktop_generation: DesktopGeneration,
    entries: &mut [WindowSnapshotEntry],
) {
    let pids = unique_reported_pids(entries.iter().map(|entry| &entry.snapshot));
    let Ok(upgrades) =
        resolve_process_correlation_upgrades(correlator, desktop_generation, pids).await
    else {
        return;
    };
    for entry in entries {
        apply_process_correlation_upgrade(&mut entry.snapshot, &upgrades);
    }
}

fn unique_reported_pids<'a>(snapshots: impl Iterator<Item = &'a WindowSnapshot>) -> Vec<u32> {
    let mut pids = Vec::new();
    for snapshot in snapshots {
        if let Some(pid) = snapshot.process.reported_pid
            && pid != 0
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
    }
    pids
}

async fn resolve_process_correlation_upgrades(
    correlator: &dyn PidCorrelator,
    desktop_generation: DesktopGeneration,
    pids: Vec<u32>,
) -> Result<BTreeMap<u32, ProcessCorrelationUpgrade>, PidCorrelationError> {
    let deadline = tokio::time::Instant::now() + PROCESS_CORRELATION_TOTAL_TIMEOUT;
    let mut upgrades = BTreeMap::new();
    for batch in pids.chunks(MAX_PROCESS_CORRELATION_PIDS) {
        let requested = batch.to_vec();
        let Ok(Ok(reply)) = tokio::time::timeout_at(
            deadline,
            correlator.correlate(desktop_generation, requested.clone()),
        )
        .await
        else {
            // Transport unavailability is normally shared by every batch;
            // one shared deadline prevents per-batch latency multiplication.
            return Err(PidCorrelationError);
        };
        let Some(batch_upgrades) =
            validate_correlation_reply(desktop_generation, &requested, reply)
        else {
            return Err(PidCorrelationError);
        };
        upgrades.extend(batch_upgrades);
    }
    Ok(upgrades)
}

#[cfg(test)]
fn apply_process_correlation_upgrade(
    snapshot: &mut WindowSnapshot,
    upgrades: &BTreeMap<u32, ProcessCorrelationUpgrade>,
) {
    let Some(reported_pid) = snapshot.process.reported_pid else {
        return;
    };
    let Some(upgrade) = upgrades.get(&reported_pid).copied() else {
        return;
    };
    snapshot.process.managed_process = Some(upgrade.process);
    snapshot.process.confidence = WindowProcessConfidence::High;
    snapshot.process.evidence = vec![WindowProcessEvidence::NetWmPid, upgrade.evidence];
    snapshot.process.conflict = false;
}

fn validate_correlation_reply(
    desktop_generation: DesktopGeneration,
    requested: &[u32],
    reply: Vec<BrokerPidCorrelation>,
) -> Option<BTreeMap<u32, ProcessCorrelationUpgrade>> {
    if reply.len() != requested.len() {
        return None;
    }
    let mut upgrades = BTreeMap::new();
    for (requested_pid, correlation) in requested.iter().copied().zip(reply) {
        if correlation.pid != requested_pid {
            return None;
        }
        let upgrade = match correlation.evidence {
            BrokerPidCorrelationEvidence::ManagedLeader { process } => {
                if process.validate().is_err()
                    || process.desktop_generation != desktop_generation
                    || process.pid != requested_pid
                {
                    return None;
                }
                Some(ProcessCorrelationUpgrade {
                    process,
                    evidence: WindowProcessEvidence::ProcStartTime,
                })
            }
            BrokerPidCorrelationEvidence::ManagedProcessGroup { process } => {
                if process.validate().is_err() || process.desktop_generation != desktop_generation {
                    return None;
                }
                Some(ProcessCorrelationUpgrade {
                    process,
                    evidence: WindowProcessEvidence::ProcessGroup,
                })
            }
            BrokerPidCorrelationEvidence::NoMatch => None,
        };
        if let Some(upgrade) = upgrade
            && upgrades.insert(requested_pid, upgrade).is_some()
        {
            return None;
        }
    }
    Some(upgrades)
}

#[cfg(test)]
async fn enrich_list_result(
    correlator: &dyn PidCorrelator,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    mut result: WindowListPage,
) -> Result<WindowListPage, ControlPlaneError> {
    if result.desktop_id != desktop_id
        || result.desktop_generation != desktop_generation
        || result.validate().is_err()
    {
        return Err(ControlPlaneError::Internal);
    }
    enrich_entries(correlator, desktop_generation, &mut result.windows).await;
    result.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(result)
}

#[cfg(test)]
async fn enrich_snapshot_result(
    correlator: &dyn PidCorrelator,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    mut result: WindowSnapshotResult,
) -> Result<WindowSnapshotResult, ControlPlaneError> {
    if result.window.snapshot.window.desktop_id != desktop_id
        || result.window.snapshot.window.desktop_generation != desktop_generation
        || result.validate().is_err()
    {
        return Err(ControlPlaneError::Internal);
    }
    enrich_entries(
        correlator,
        desktop_generation,
        std::slice::from_mut(&mut result.window),
    )
    .await;
    result.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(result)
}

#[cfg(test)]
async fn enrich_query_result(
    correlator: &dyn PidCorrelator,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    mut result: WindowQueryPage,
) -> Result<WindowQueryPage, ControlPlaneError> {
    if result.desktop_id != desktop_id
        || result.desktop_generation != desktop_generation
        || result.validate().is_err()
    {
        return Err(ControlPlaneError::Internal);
    }
    enrich_entries(correlator, desktop_generation, &mut result.windows).await;
    result.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(result)
}

#[cfg(test)]
async fn enrich_resolve_result(
    correlator: &dyn PidCorrelator,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    mut result: WindowResolveResult,
) -> Result<WindowResolveResult, ControlPlaneError> {
    if result.desktop_id != desktop_id
        || result.desktop_generation != desktop_generation
        || result.validate().is_err()
    {
        return Err(ControlPlaneError::Internal);
    }
    enrich_entries(
        correlator,
        desktop_generation,
        std::slice::from_mut(&mut result.window),
    )
    .await;
    result.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(result)
}

#[cfg(test)]
async fn enrich_wait_result(
    correlator: &dyn PidCorrelator,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    mut result: WindowWaitResult,
) -> Result<WindowWaitResult, ControlPlaneError> {
    if result.desktop_id != desktop_id
        || result.desktop_generation != desktop_generation
        || result.validate().is_err()
    {
        return Err(ControlPlaneError::Internal);
    }
    enrich_entries(correlator, desktop_generation, &mut result.windows).await;
    result.validate().map_err(|_| ControlPlaneError::Internal)?;
    Ok(result)
}

impl ObservationPlane for DaemonObservationService {
    fn list_windows<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowListRequest,
    ) -> ObservationFuture<'a, Result<WindowListPage, ControlPlaneError>> {
        Box::pin(async move {
            let principal = authorized_principal(&context)?;
            self.list_for_principal(principal, request).await
        })
    }

    fn window_snapshot<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowSnapshotRequest,
    ) -> ObservationFuture<'a, Result<WindowSnapshotResult, ControlPlaneError>> {
        Box::pin(async move {
            let principal = authorized_principal(&context)?;
            self.snapshot_for_principal(principal, request).await
        })
    }

    fn query_windows<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowQueryRequest,
    ) -> ObservationFuture<'a, Result<WindowQueryPage, ControlPlaneError>> {
        Box::pin(async move {
            let principal = authorized_principal(&context)?;
            self.query_for_principal(principal, request).await
        })
    }

    fn resolve_window<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowResolveRequest,
    ) -> ObservationFuture<'a, Result<WindowResolveResult, ControlPlaneError>> {
        Box::pin(async move {
            let principal = authorized_principal(&context)?;
            self.resolve_for_principal(principal, request).await
        })
    }

    fn wait_window<'a>(
        &'a self,
        context: ControlRequestContext,
        request: WindowWaitRequest,
    ) -> ObservationFuture<'a, Result<WindowWaitResult, ControlPlaneError>> {
        Box::pin(async move {
            let principal = authorized_principal(&context)?;
            self.wait_for_principal(principal, request).await
        })
    }
}

fn authorized_principal(context: &ControlRequestContext) -> Result<String, ControlPlaneError> {
    if !context.principal().has_grant(Grant::DesktopObserve) {
        return Err(ControlPlaneError::PermissionDenied);
    }
    Ok(context.principal().id().to_owned())
}

#[allow(dead_code, reason = "some internal requests await coordinator wiring")]
enum ModelRequest {
    List {
        principal: String,
        request: WindowListRequest,
        response: oneshot::Sender<Result<WindowListPage, ControlPlaneError>>,
    },
    Snapshot {
        principal: String,
        request: WindowSnapshotRequest,
        response: oneshot::Sender<Result<WindowSnapshotResult, ControlPlaneError>>,
    },
    Query {
        principal: String,
        request: WindowQueryRequest,
        response: oneshot::Sender<Result<WindowQueryPage, ControlPlaneError>>,
    },
    Resolve {
        principal: String,
        request: WindowResolveRequest,
        response: oneshot::Sender<Result<WindowResolveResult, ControlPlaneError>>,
    },
    Wait {
        principal: String,
        request: WindowWaitRequest,
        deadline: Option<Instant>,
        response: oneshot::Sender<Result<WindowWaitResult, ControlPlaneError>>,
    },
    Revalidate {
        window: WindowRef,
        response: SyncSender<Result<u32, ControlPlaneError>>,
    },
    InternalSnapshot {
        window: WindowRef,
        response: SyncSender<Result<WindowSnapshot, ControlPlaneError>>,
    },
    AccessibilityCorrelationSnapshot {
        response: oneshot::Sender<Result<ObservationCorrelationSnapshot, ControlPlaneError>>,
    },
    AccessibilityCorrelationSnapshotBlocking {
        response: SyncSender<Result<ObservationCorrelationSnapshot, ControlPlaneError>>,
    },
    ProcessCorrelationSnapshot {
        response: oneshot::Sender<Result<ObservationCorrelationSnapshot, ControlPlaneError>>,
    },
    ReplaceProcessCorrelations {
        expected_revision: WindowModelRevision,
        expected_correlation_epoch: u64,
        expected_generation: DesktopGeneration,
        expected_lifecycle_authority: ProcessLifecycleAuthorityFence,
        assignments: Vec<ProcessCorrelationAssignment>,
        response: oneshot::Sender<Result<ProcessCorrelationCommit, ControlPlaneError>>,
    },
    InvalidateProcessCorrelations {
        response: oneshot::Sender<Result<ProcessCorrelationFence, ControlPlaneError>>,
    },
    OcclusionSnapshot {
        window: WindowRef,
        response: SyncSender<Result<WindowOcclusionSnapshot, ControlPlaneError>>,
    },
    ReplaceAccessibilityCorrelations {
        expected_revision: WindowModelRevision,
        windows: Vec<WindowRef>,
        response: oneshot::Sender<Result<WindowModelRevision, ControlPlaneError>>,
    },
}

impl ModelRequest {
    fn fail(self, error: ControlPlaneError) {
        match self {
            Self::List { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::Snapshot { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::Query { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::Resolve { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::Wait { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::Revalidate { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::InternalSnapshot { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::AccessibilityCorrelationSnapshot { response } => {
                let _ = response.send(Err(error));
            }
            Self::AccessibilityCorrelationSnapshotBlocking { response } => {
                let _ = response.send(Err(error));
            }
            Self::ProcessCorrelationSnapshot { response } => {
                let _ = response.send(Err(error));
            }
            Self::ReplaceProcessCorrelations { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::InvalidateProcessCorrelations { response } => {
                let _ = response.send(Err(error));
            }
            Self::OcclusionSnapshot { response, .. } => {
                let _ = response.send(Err(error));
            }
            Self::ReplaceAccessibilityCorrelations { response, .. } => {
                let _ = response.send(Err(error));
            }
        }
    }
}

struct PendingWait {
    principal: String,
    request: WindowWaitRequest,
    deadline: MonotonicMillis,
    wall_deadline: Option<Instant>,
    response: oneshot::Sender<Result<WindowWaitResult, ControlPlaneError>>,
}

struct ModelActorControl {
    shutdown: AtomicBool,
    state: AtomicU8,
    sequence: Mutex<u64>,
    wake: Condvar,
}

impl Default for ModelActorControl {
    fn default() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            state: AtomicU8::new(0),
            sequence: Mutex::new(0),
            wake: Condvar::new(),
        }
    }
}

impl ModelActorControl {
    fn mark_healthy(&self) {
        self.state.store(1, Ordering::Release);
    }

    fn record_exit(&self, exit: ObservationServiceExit) {
        let state = match exit {
            ObservationServiceExit::Stopped => 2,
            ObservationServiceExit::Poisoned => 3,
            ObservationServiceExit::Panicked => 4,
        };
        self.state.store(state, Ordering::Release);
    }

    fn health(&self) -> ObservationServiceState {
        match self.state.load(Ordering::Acquire) {
            0 => ObservationServiceState::Starting,
            1 => ObservationServiceState::Healthy,
            2 => ObservationServiceState::Stopped,
            3 => ObservationServiceState::Poisoned,
            _ => ObservationServiceState::Panicked,
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.notify();
    }

    fn notify(&self) {
        let mut sequence = lock_unpoisoned(&self.sequence);
        *sequence = sequence.wrapping_add(1);
        self.wake.notify_one();
    }

    fn wait(&self, observed: u64, timeout: Duration) -> u64 {
        let sequence = lock_unpoisoned(&self.sequence);
        if *sequence != observed {
            return *sequence;
        }
        let (sequence, _) = self
            .wake
            .wait_timeout(sequence, timeout)
            .unwrap_or_else(|error| error.into_inner());
        *sequence
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[allow(clippy::type_complexity)]
#[allow(dead_code)] // Shared by compatibility composition and deterministic actor tests.
fn spawn_model_actor(
    backend: Box<dyn RawObservationBackend>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    spawn_model_actor_with_correlator(
        backend,
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
        Arc::new(UnavailablePidCorrelator),
    )
}

#[allow(clippy::type_complexity)]
fn spawn_model_actor_with_correlator(
    backend: Box<dyn RawObservationBackend>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    pid_correlator: Arc<dyn PidCorrelator>,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    spawn_model_actor_with_components(
        backend,
        desktop_id,
        desktop_generation,
        model_limits,
        settings,
        pid_correlator,
        Arc::new(UnavailableWindowEventSink),
    )
}

#[allow(clippy::type_complexity)]
fn spawn_model_actor_with_components(
    backend: Box<dyn RawObservationBackend>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    pid_correlator: Arc<dyn PidCorrelator>,
    event_sink: Arc<dyn WindowEventSink>,
) -> Result<
    (
        Arc<DaemonObservationService>,
        ObservationServiceShutdown,
        ObservationServiceJoin,
    ),
    ObservationCompositionError,
> {
    let settings = settings.validate()?;
    let wait_terminal_settlement_timeout = settings
        .raw_request_timeout
        .checked_add(settings.idle_poll_interval)
        .and_then(|timeout| timeout.checked_add(Duration::from_millis(250)))
        .ok_or(ObservationCompositionError::InvalidSettings)?;
    let (request_tx, request_rx) = mpsc::sync_channel(settings.request_capacity);
    let control = Arc::new(ModelActorControl::default());
    let thread_control = Arc::clone(&control);
    let exit_control = Arc::clone(&control);
    let process_refresh = Arc::new(ProcessCorrelationRefreshCoordinator::new());
    let model_changed = Arc::clone(&process_refresh.model_changed);
    let process_lifecycle_authority = Arc::new(ProcessLifecycleAuthority::new());
    let thread_process_lifecycle_authority = Arc::clone(&process_lifecycle_authority);
    let event_metrics = Arc::new(WindowEventDeliveryMetrics::default());
    let thread_event_metrics = Arc::clone(&event_metrics);
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("xenoteer-window-model".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_model_actor(
                    backend,
                    request_rx,
                    thread_control,
                    desktop_id,
                    desktop_generation,
                    model_limits,
                    settings,
                    startup_tx,
                    event_sink,
                    thread_event_metrics,
                    model_changed,
                    thread_process_lifecycle_authority,
                )
            }));
            let exit = result.unwrap_or(ObservationServiceExit::Panicked);
            exit_control.record_exit(exit);
            exit
        })
        .map_err(|_| ObservationCompositionError::ThreadSpawnFailed)?;
    let startup = startup_rx.recv_timeout(settings.startup_timeout);
    if !matches!(startup, Ok(Ok(()))) {
        control.request_shutdown();
        let _ = thread.join();
        return Err(ObservationCompositionError::InitialReconcileFailed);
    }
    let shutdown = ObservationServiceShutdown {
        control: Arc::clone(&control),
    };
    let service = Arc::new(DaemonObservationService {
        requests: request_tx,
        control,
        desktop_generation,
        pid_correlator,
        process_refresh,
        process_lifecycle_authority,
        wait_terminal_settlement_timeout,
        event_metrics,
        #[cfg(test)]
        unavailable_refresh_hook: Mutex::new(None),
    });
    Ok((
        service,
        shutdown.clone(),
        ObservationServiceJoin {
            thread: Some(thread),
            shutdown,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_model_actor(
    mut backend: Box<dyn RawObservationBackend>,
    requests: Receiver<ModelRequest>,
    control: Arc<ModelActorControl>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    model_limits: WindowModelLimits,
    settings: ObservationServiceSettings,
    startup: SyncSender<Result<(), ObservationCompositionError>>,
    event_sink: Arc<dyn WindowEventSink>,
    event_metrics: Arc<WindowEventDeliveryMetrics>,
    model_changed: Arc<ProcessCorrelationModelChanges>,
    process_lifecycle_authority: Arc<ProcessLifecycleAuthority>,
) -> ObservationServiceExit {
    let epoch = Instant::now();
    let mut state = match ModelState::new_with_event_components(
        desktop_id,
        desktop_generation,
        model_limits,
        settings.token_capacity,
        settings.cursor_ttl_ms,
        settings.reference_ttl_ms,
        event_sink,
        event_metrics,
    ) {
        Ok(state) => state,
        Err(_) => {
            let _ = startup.send(Err(ObservationCompositionError::InitialReconcileFailed));
            let _ = backend.shutdown(settings.raw_request_timeout);
            return ObservationServiceExit::Poisoned;
        }
    };
    state.model_changes = Some(Arc::clone(&model_changed));
    state.process_lifecycle_authority = process_lifecycle_authority;
    let startup_deadline = match Instant::now().checked_add(settings.raw_request_timeout) {
        Some(deadline) => deadline,
        None => {
            let _ = startup.send(Err(ObservationCompositionError::InitialReconcileFailed));
            let _ = backend.shutdown(settings.raw_request_timeout);
            return ObservationServiceExit::Poisoned;
        }
    };
    if let Err(error) = reconcile_from_backend_until(
        &mut state,
        backend.as_mut(),
        &control,
        &|| monotonic_now(epoch),
        startup_deadline,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    ) {
        tracing::error!(?error, "initial observation reconciliation failed");
        let _ = startup.send(Err(ObservationCompositionError::InitialReconcileFailed));
        let _ = backend.shutdown(settings.raw_request_timeout);
        return ObservationServiceExit::Poisoned;
    }
    control.mark_healthy();
    if startup.send(Ok(())).is_err() {
        control.request_shutdown();
    }

    let mut waiters = Vec::new();
    let mut wake_sequence = 0;
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            fail_pending(
                &requests,
                &mut waiters,
                ControlPlaneError::CapabilityUnavailable,
            );
            let _ = backend.shutdown(settings.raw_request_timeout);
            return ObservationServiceExit::Stopped;
        }
        let now = monotonic_now(epoch);
        process_waiters(&mut state, &mut waiters, now);
        let mut did_work = false;
        for _ in 0..64 {
            if control.shutdown.load(Ordering::Acquire) {
                break;
            }
            match backend.try_event() {
                Ok(Some(event)) => {
                    did_work = true;
                    if let Err(error) = process_actor_event(
                        &mut state,
                        &mut waiters,
                        backend.as_mut(),
                        &control,
                        event,
                        &|| monotonic_now(epoch),
                        settings.raw_request_timeout,
                    ) {
                        if control.shutdown.load(Ordering::Acquire) {
                            fail_pending(
                                &requests,
                                &mut waiters,
                                ControlPlaneError::CapabilityUnavailable,
                            );
                            let _ = backend.shutdown(settings.raw_request_timeout);
                            return ObservationServiceExit::Stopped;
                        }
                        tracing::error!(?error, "observation event processing failed");
                        fail_pending(
                            &requests,
                            &mut waiters,
                            ControlPlaneError::CapabilityUnavailable,
                        );
                        let _ = backend.shutdown(settings.raw_request_timeout);
                        return ObservationServiceExit::Poisoned;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    if control.shutdown.load(Ordering::Acquire) {
                        fail_pending(
                            &requests,
                            &mut waiters,
                            ControlPlaneError::CapabilityUnavailable,
                        );
                        let _ = backend.shutdown(settings.raw_request_timeout);
                        return ObservationServiceExit::Stopped;
                    }
                    tracing::error!(?error, "raw observation event source failed");
                    fail_pending(
                        &requests,
                        &mut waiters,
                        ControlPlaneError::CapabilityUnavailable,
                    );
                    let _ = backend.shutdown(settings.raw_request_timeout);
                    return ObservationServiceExit::Poisoned;
                }
            }
        }
        for _ in 0..32 {
            match requests.try_recv() {
                Ok(request) => {
                    did_work = true;
                    process_model_request(
                        &mut state,
                        &mut waiters,
                        request,
                        monotonic_now(epoch),
                        settings.max_waiters,
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    fail_pending(
                        &requests,
                        &mut waiters,
                        ControlPlaneError::CapabilityUnavailable,
                    );
                    let _ = backend.shutdown(settings.raw_request_timeout);
                    return ObservationServiceExit::Stopped;
                }
            }
        }
        if !did_work {
            wake_sequence = control.wait(wake_sequence, settings.idle_poll_interval);
        }
    }
}

fn process_model_request(
    state: &mut ModelState,
    waiters: &mut Vec<PendingWait>,
    request: ModelRequest,
    now: MonotonicMillis,
    max_waiters: usize,
) {
    match request {
        ModelRequest::List {
            principal,
            request,
            response,
        } => {
            let _ = response.send(state.list(&principal, &request, now));
        }
        ModelRequest::Snapshot {
            principal,
            request,
            response,
        } => {
            let _ = response.send(state.snapshot(&principal, &request, now));
        }
        ModelRequest::Query {
            principal,
            request,
            response,
        } => {
            let _ = response.send(state.query(&principal, &request, now));
        }
        ModelRequest::Resolve {
            principal,
            request,
            response,
        } => {
            let _ = response.send(state.resolve(&principal, &request, now));
        }
        ModelRequest::Wait {
            principal,
            request,
            deadline,
            response,
        } => {
            if response.is_closed() {
                return;
            }
            if deadline.is_some_and(|deadline| Instant::now() > deadline) {
                let _ = response.send(state.timed_out_wait(&principal, &request, now));
                return;
            }
            let wall_deadline = deadline;
            match state.evaluate_wait(&principal, &request, now, None) {
                Ok(Some(result)) => {
                    let _ = response.send(Ok(result));
                }
                Err(error) => {
                    let _ = response.send(Err(error));
                }
                Ok(None) => {
                    waiters.retain(|waiter| !waiter.response.is_closed());
                    if waiters.len() >= max_waiters {
                        let _ = response.send(Err(ControlPlaneError::ResourceExhausted));
                        return;
                    }
                    let remaining_ms = deadline.map_or(u64::from(request.timeout_ms), |deadline| {
                        u64::try_from(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis()
                                .saturating_add(1),
                        )
                        .unwrap_or(u64::MAX)
                    });
                    let Some(deadline) = now.checked_add(remaining_ms) else {
                        let _ = response.send(Err(ControlPlaneError::InvalidRequest));
                        return;
                    };
                    waiters.push(PendingWait {
                        principal,
                        request,
                        deadline,
                        wall_deadline,
                        response,
                    });
                    // The actor is the sole model owner, so this immediate
                    // second evaluation closes the check/register window.
                    process_waiters(state, waiters, now);
                }
            }
        }
        ModelRequest::Revalidate { window, response } => {
            let result = state
                .model
                .resolve_exact(&window, now)
                .map(|resolved| resolved.snapshot.window.xid)
                .map_err(map_model_error);
            let _ = response.send(result);
        }
        ModelRequest::InternalSnapshot { window, response } => {
            let result = state
                .model
                .resolve_exact(&window, now)
                .map(|resolved| resolved.snapshot)
                .map_err(map_model_error);
            let _ = response.send(result);
        }
        ModelRequest::AccessibilityCorrelationSnapshot { response } => {
            let _ = response.send(state.correlation_snapshot(now));
        }
        ModelRequest::AccessibilityCorrelationSnapshotBlocking { response } => {
            let _ = response.send(state.correlation_snapshot(now));
        }
        ModelRequest::ProcessCorrelationSnapshot { response } => {
            let _ = response.send(state.correlation_snapshot(now));
        }
        ModelRequest::ReplaceProcessCorrelations {
            expected_revision,
            expected_correlation_epoch,
            expected_generation,
            expected_lifecycle_authority,
            assignments,
            response,
        } => {
            let result = if state
                .process_lifecycle_authority
                .accepts(expected_lifecycle_authority)
            {
                state
                    .replace_process_correlations(
                        expected_revision,
                        expected_correlation_epoch,
                        expected_generation,
                        &assignments,
                        now,
                    )
                    .map(|fence| ProcessCorrelationCommit {
                        fence,
                        change_sequence: state.model_change_sequence(),
                    })
                    .inspect(|_| process_waiters(state, waiters, now))
            } else {
                Err(ControlPlaneError::CapabilityUnavailable)
            };
            let _ = response.send(result);
        }
        ModelRequest::InvalidateProcessCorrelations { response } => {
            let result = state
                .invalidate_process_correlations(now)
                .inspect(|_| process_waiters(state, waiters, now));
            let _ = response.send(result);
        }
        ModelRequest::OcclusionSnapshot { window, response } => {
            let _ = response.send(state.occlusion_snapshot(&window, now));
        }
        ModelRequest::ReplaceAccessibilityCorrelations {
            expected_revision,
            windows,
            response,
        } => {
            let changed = state
                .replace_accessibility_correlations(expected_revision, &windows, now)
                .inspect(|_| process_waiters(state, waiters, now));
            let _ = response.send(changed);
        }
    }
}

fn process_waiters(state: &mut ModelState, waiters: &mut Vec<PendingWait>, now: MonotonicMillis) {
    let mut retained = Vec::with_capacity(waiters.len());
    for waiter in std::mem::take(waiters) {
        if waiter.response.is_closed() {
            continue;
        }
        let wall_deadline_order = waiter
            .wall_deadline
            .map(|deadline| Instant::now().cmp(&deadline));
        if now > waiter.deadline || wall_deadline_order.is_some_and(|order| order.is_gt()) {
            let result = state.timed_out_wait(&waiter.principal, &waiter.request, now);
            let _ = waiter.response.send(result);
            continue;
        }
        let at_deadline =
            now == waiter.deadline || wall_deadline_order.is_some_and(|order| order.is_eq());
        match state.evaluate_wait(&waiter.principal, &waiter.request, now, None) {
            Ok(Some(result)) => {
                let _ = waiter.response.send(Ok(result));
            }
            Err(error) => {
                let _ = waiter.response.send(Err(error));
            }
            Ok(None) if at_deadline => {
                let result = state.timed_out_wait(&waiter.principal, &waiter.request, now);
                let _ = waiter.response.send(result);
            }
            Ok(None) => retained.push(waiter),
        }
    }
    *waiters = retained;
}

fn complete_resync_waiters(
    state: &mut ModelState,
    waiters: &mut Vec<PendingWait>,
    now: MonotonicMillis,
) {
    for waiter in std::mem::take(waiters) {
        if waiter.response.is_closed() {
            continue;
        }
        let result = state
            .evaluate_wait(
                &waiter.principal,
                &waiter.request,
                now,
                Some(WindowWaitStatus::ResyncRequired),
            )
            .and_then(|result| result.ok_or(ControlPlaneError::Internal));
        let _ = waiter.response.send(result);
    }
}

fn fail_pending(
    requests: &Receiver<ModelRequest>,
    waiters: &mut Vec<PendingWait>,
    error: ControlPlaneError,
) {
    while let Ok(request) = requests.try_recv() {
        request.fail(error);
    }
    for waiter in std::mem::take(waiters) {
        let _ = waiter.response.send(Err(error));
    }
}

fn process_actor_event(
    state: &mut ModelState,
    waiters: &mut Vec<PendingWait>,
    backend: &mut dyn RawObservationBackend,
    control: &ModelActorControl,
    event: ObservationActorEvent,
    now: &dyn Fn() -> MonotonicMillis,
    timeout: Duration,
) -> Result<(), ObservationAdapterError> {
    let resync = matches!(&event, ObservationActorEvent::ResyncRequired);
    let before_revision = state.model.revision();
    let before_correlation_epoch = state.correlation_epoch;
    process_raw_event(state, backend, control, event, now, timeout)?;
    if state.model.revision() != before_revision
        || state.correlation_epoch != before_correlation_epoch
    {
        state.publish_model_change();
    }
    let now = now();
    if resync {
        complete_resync_waiters(state, waiters, now);
    } else {
        process_waiters(state, waiters, now);
    }
    Ok(())
}

fn process_raw_event(
    state: &mut ModelState,
    backend: &mut dyn RawObservationBackend,
    control: &ModelActorControl,
    event: ObservationActorEvent,
    now: &dyn Fn() -> MonotonicMillis,
    timeout: Duration,
) -> Result<(), ObservationAdapterError> {
    let raw_deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ObservationAdapterError::ClockOverflow)?;
    let (decision, identity_policy, event_policy) = match event {
        ObservationActorEvent::Reconcile { decision } => {
            let event_policy = if decision == ReconcileDecision::FullResync {
                ReconcileEventPolicy::Rebuilt(WindowModelRebuildReason::ExplicitResync)
            } else {
                ReconcileEventPolicy::Incremental
            };
            (
                decision,
                ReconcileIdentityPolicy::PreserveContinuity,
                event_policy,
            )
        }
        ObservationActorEvent::ResyncRequired => {
            // The model rebuild repairs server-side state, but the external
            // event stream still lost an unknown interval. Publish only the
            // coordinator's metadata-free barrier so every active subscriber
            // fails closed instead of inferring continuity from the rebuild.
            state.events.require_resync();
            (
                ReconcileDecision::FullResync,
                ReconcileIdentityPolicy::InvalidateAll,
                ReconcileEventPolicy::Rebuilt(WindowModelRebuildReason::EventOverflow),
            )
        }
        ObservationActorEvent::RootDamaged { damage } => {
            state.emit_root_damage(damage);
            return Ok(());
        }
        // The X11 actor pairs this nonterminal diagnostic with one
        // ResyncRequired marker. Waiting for that marker prevents one gap from
        // causing two identity transitions.
        ObservationActorEvent::Failed { failure }
            if failure.kind == ObservationActorFailureKind::RequestFailed =>
        {
            return Ok(());
        }
        ObservationActorEvent::Failed { .. } => {
            return Err(ObservationAdapterError::Model);
        }
    };
    match decision {
        ReconcileDecision::ObserveWindow { window }
        | ReconcileDecision::RefreshWindow { window, .. } => {
            match backend.snapshot(window, control, remaining_raw_event_budget(raw_deadline)?) {
                Ok(input) => {
                    remaining_raw_event_budget(raw_deadline)?;
                    state.observe_raw(&input, now())
                }
                Err(RawBackendFailure::RequestFailed) => reconcile_from_backend_until(
                    state,
                    backend,
                    control,
                    now,
                    raw_deadline,
                    ReconcileIdentityPolicy::PreserveContinuity,
                    ReconcileEventPolicy::Incremental,
                ),
                Err(_) => Err(ObservationAdapterError::Model),
            }
        }
        ReconcileDecision::RemoveWindow { window } => state.remove_xid(window, now()),
        ReconcileDecision::RefreshFocus
        | ReconcileDecision::RebuildInventory
        | ReconcileDecision::FullResync => reconcile_from_backend_until(
            state,
            backend,
            control,
            now,
            raw_deadline,
            identity_policy,
            event_policy,
        ),
        ReconcileDecision::ConnectionFailed => Err(ObservationAdapterError::Model),
        ReconcileDecision::Ignore => Ok(()),
    }
}

#[cfg(test)]
fn reconcile_from_backend(
    state: &mut ModelState,
    backend: &mut dyn RawObservationBackend,
    control: &ModelActorControl,
    now: MonotonicMillis,
    timeout: Duration,
    identity_policy: ReconcileIdentityPolicy,
    event_policy: ReconcileEventPolicy,
) -> Result<(), ObservationAdapterError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ObservationAdapterError::ClockOverflow)?;
    reconcile_from_backend_until(
        state,
        backend,
        control,
        &|| now,
        deadline,
        identity_policy,
        event_policy,
    )
}

fn reconcile_from_backend_until(
    state: &mut ModelState,
    backend: &mut dyn RawObservationBackend,
    control: &ModelActorControl,
    now: &dyn Fn() -> MonotonicMillis,
    deadline: Instant,
    identity_policy: ReconcileIdentityPolicy,
    event_policy: ReconcileEventPolicy,
) -> Result<(), ObservationAdapterError> {
    let mut inventory = backend
        .reconcile(control, remaining_raw_event_budget(deadline)?)
        .map_err(|_| ObservationAdapterError::Model)?;
    remaining_raw_event_budget(deadline)?;
    let mut inputs = Vec::with_capacity(inventory.windows.len());
    let candidate_windows = std::mem::take(&mut inventory.windows);
    for window in candidate_windows {
        let snapshot =
            match backend.snapshot(window, control, remaining_raw_event_budget(deadline)?) {
                Ok(snapshot) => Some(snapshot),
                Err(RawBackendFailure::RequestFailed) => {
                    match backend.snapshot(window, control, remaining_raw_event_budget(deadline)?) {
                        Ok(snapshot) => Some(snapshot),
                        Err(RawBackendFailure::RequestFailed) => None,
                        Err(_) => return Err(ObservationAdapterError::Model),
                    }
                }
                Err(_) => return Err(ObservationAdapterError::Model),
            };
        remaining_raw_event_budget(deadline)?;
        if let Some(snapshot) = snapshot {
            inventory.windows.push(window);
            inputs.push(snapshot);
        } else if !inventory
            .warnings
            .contains(&InventoryWarning::VanishedMember)
        {
            inventory.warnings.push(InventoryWarning::VanishedMember);
        }
    }
    remaining_raw_event_budget(deadline)?;
    state.reconcile_raw_with_event_policy(&inventory, &inputs, now(), identity_policy, event_policy)
}

fn remaining_raw_event_budget(deadline: Instant) -> Result<Duration, ObservationAdapterError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(ObservationAdapterError::Model)?;
    if remaining.is_zero() {
        return Err(ObservationAdapterError::Model);
    }
    Ok(remaining)
}

fn monotonic_now(epoch: Instant) -> MonotonicMillis {
    let millis = epoch.elapsed().as_millis();
    MonotonicMillis::new(u64::try_from(millis).unwrap_or(u64::MAX))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawBackendFailure {
    RequestFailed,
    Unavailable,
    TimedOut,
    Stopped,
}

trait RawObservationBackend: Send + 'static {
    fn reconcile(
        &mut self,
        control: &ModelActorControl,
        timeout: Duration,
    ) -> Result<RootInventory, RawBackendFailure>;
    fn snapshot(
        &mut self,
        window: u32,
        control: &ModelActorControl,
        timeout: Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure>;
    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure>;
    fn shutdown(&mut self, timeout: Duration) -> ObservationActorExit;
}

struct LiveRawBackend {
    handle: ObservationActorHandle,
    events: ObservationActorEventReceiver,
    join: Option<ObservationActorJoin>,
}

impl LiveRawBackend {
    fn new(
        handle: ObservationActorHandle,
        events: ObservationActorEventReceiver,
        join: ObservationActorJoin,
    ) -> Self {
        Self {
            handle,
            events,
            join: Some(join),
        }
    }
}

impl RawObservationBackend for LiveRawBackend {
    fn reconcile(
        &mut self,
        control: &ModelActorControl,
        timeout: Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        let reply = self.handle.try_reconcile().map_err(map_raw_submit_error)?;
        wait_raw_reply(&reply, control, timeout, true)
    }

    fn snapshot(
        &mut self,
        window: u32,
        control: &ModelActorControl,
        timeout: Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        let reply = self
            .handle
            .try_snapshot(window)
            .map_err(map_raw_submit_error)?;
        wait_raw_reply(&reply, control, timeout, true)
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(RawBackendFailure::Unavailable),
        }
    }

    fn shutdown(&mut self, timeout: Duration) -> ObservationActorExit {
        if let Some(join) = self.join.take() {
            let reply = self.handle.shutdown();
            let control = ModelActorControl::default();
            let _ = wait_raw_reply(&reply, &control, timeout, false);
            join.join()
        } else {
            ObservationActorExit::Stopped
        }
    }
}

impl Drop for LiveRawBackend {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_secs(2));
    }
}

fn map_raw_submit_error(error: ObservationActorSubmitError) -> RawBackendFailure {
    match error {
        ObservationActorSubmitError::QueueFull => RawBackendFailure::Unavailable,
        ObservationActorSubmitError::Closed => RawBackendFailure::Stopped,
    }
}

fn wait_raw_reply<T>(
    reply: &ObservationReply<T>,
    control: &ModelActorControl,
    timeout: Duration,
    observe_shutdown: bool,
) -> Result<T, RawBackendFailure> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(RawBackendFailure::TimedOut)?;
    loop {
        if observe_shutdown && control.shutdown.load(Ordering::Acquire) {
            return Err(RawBackendFailure::Stopped);
        }
        match reply.try_recv() {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(failure)) => {
                return Err(match failure.kind {
                    ObservationActorFailureKind::RequestFailed => RawBackendFailure::RequestFailed,
                    ObservationActorFailureKind::ActorStopped => RawBackendFailure::Stopped,
                    _ => RawBackendFailure::Unavailable,
                });
            }
            Err(TryRecvError::Disconnected) => return Err(RawBackendFailure::Unavailable),
            Err(TryRecvError::Empty) if Instant::now() >= deadline => {
                return Err(RawBackendFailure::TimedOut);
            }
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[cfg(test)]
#[path = "observation_plane_correlation_tests.rs"]
mod correlation_tests;

#[cfg(test)]
#[path = "observation_plane_event_tests.rs"]
mod event_tests;

#[cfg(test)]
#[path = "observation_plane_live_tests.rs"]
mod live_tests;

#[cfg(test)]
#[path = "observation_plane_tests.rs"]
mod tests;
