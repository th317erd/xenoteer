//! Authenticated, bounded accessibility reads over the daemon-owned cache.
//!
//! The plane deliberately has one mutable lock. AT-SPI ingestion, cursor
//! consumption, and wait registration therefore share one serialization
//! point; immutable core snapshots do the bounded query work.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::{self, Write as _},
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout_at};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xenoteer_atspi::{
    AtspiHandle, CacheLimits, CacheMutation, CacheMutationDetail, CacheMutationKind, CachePage,
    CachedNode, ObjectAddress, SemanticError, SemanticObservationRequest,
    SemanticObservationResult, SemanticRect, SemanticTargetRequest, TextProtection,
};
use xenoteer_core::{
    AccessibilityCache, AccessibilityContinuationDescriptor, AccessibilityCorrelationError,
    AccessibilityCorrelationLimits, AccessibilityCorrelationSubject, AccessibilityModelError,
    AccessibilityModelLimits, AccessibilityQueryDeadline, AccessibilityQueryError,
    AccessibilityQueryProjection, AccessibilityWaitEvaluation, AccessibilityWindowCandidate,
    ElementClickObservation, MAX_ACCESSIBILITY_CORRELATION_CANDIDATES, MonotonicMillis,
    NormalizedCorrelationText, QueryLimit, correlate_accessibility_window,
};
use xenoteer_protocol::{
    ACCESSIBILITY_CURSOR_TTL_MS, AccessibilityIdentityHash, AccessibilityPageCursor,
    AccessibilityQueryLimits, AccessibilityRevision, ApplicationRef, AtspiBusName, AtspiGeneration,
    AtspiObjectPath, CoordinateSpace, DesktopGeneration, DesktopId, ElementCompleteness,
    ElementComponentSnapshot, ElementInterface, ElementListPage, ElementListRequest, ElementOrder,
    ElementPredicate, ElementQueryPage, ElementQueryRequest, ElementRef, ElementResolveRequest,
    ElementResolveResult, ElementRole, ElementRoleSnapshot, ElementSelector, ElementSnapshot,
    ElementSnapshotEntry, ElementSnapshotExpansion, ElementSnapshotRequest, ElementSnapshotResult,
    ElementState, ElementTextRange, ElementTextSnapshot, ElementValueSnapshot,
    ElementWaitPredicate, ElementWaitRequest, ElementWaitResult, ElementWaitStatus,
    ElementWaitTarget, ElementWindowCorrelation, MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL,
    MAX_ACCESSIBILITY_QUERY_MATCHES, MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS,
    MAX_ACCESSIBILITY_QUERY_VISITED_NODES, MAX_ACCESSIBILITY_SELECTOR_DEPTH,
    MAX_ACCESSIBILITY_SNAPSHOT_BYTES, MAX_ACCESSIBILITY_SNAPSHOT_NODES, Rect,
    WindowCorrelationConfidence, WindowModelRevision, WindowRef,
};
use xenoteer_server::{
    AccessibilityFuture, AccessibilityPlane, AccessibilityPlaneError, ControlPlaneError,
    ControlRequestContext,
};

use crate::observation_plane::{DaemonObservationService, ObservationCorrelationSnapshot};

const DEFAULT_MAX_TOTAL_CURSORS: usize = 4_096;
const DEFAULT_MAX_PENDING_WAITS: usize = 256;
const DEFAULT_MAX_PENDING_READS: usize = 64;
const DEFAULT_MAX_PENDING_POLLS: usize = 32;
const DEFAULT_MAX_BOOTSTRAP_BYTES: usize = 128 * 1_024 * 1_024;
const DEFAULT_POLL_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_POLL_MAXIMUM_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_POLL_MAX_ATTEMPTS: usize = 8;
const CURSOR_TOKEN_DIGEST_DOMAIN: &[u8] = b"xenoteer-daemon-accessibility-cursor-token-v1\0";

/// Fixed admission bounds for the daemon accessibility plane.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AccessibilityPlaneConfig {
    pub(crate) model_limits: AccessibilityModelLimits,
    pub(crate) raw_cache_limits: CacheLimits,
    pub(crate) cursor_ttl: Duration,
    pub(crate) max_total_cursors: usize,
    pub(crate) max_cursors_per_principal: usize,
    pub(crate) max_pending_waits: usize,
    pub(crate) max_pending_reads: usize,
    pub(crate) max_bootstrap_bytes: usize,
    pub(crate) max_nodes_per_query: u32,
    pub(crate) max_selector_depth: u16,
    pub(crate) max_query_matches: u16,
    pub(crate) query_timeout_ms: u32,
    pub(crate) max_snapshot_nodes: u32,
    pub(crate) max_snapshot_bytes: u32,
}

impl Default for AccessibilityPlaneConfig {
    fn default() -> Self {
        Self {
            model_limits: AccessibilityModelLimits::default(),
            raw_cache_limits: CacheLimits::default(),
            cursor_ttl: Duration::from_millis(u64::from(ACCESSIBILITY_CURSOR_TTL_MS)),
            max_total_cursors: DEFAULT_MAX_TOTAL_CURSORS,
            max_cursors_per_principal: MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL,
            max_pending_waits: DEFAULT_MAX_PENDING_WAITS,
            max_pending_reads: DEFAULT_MAX_PENDING_READS,
            max_bootstrap_bytes: DEFAULT_MAX_BOOTSTRAP_BYTES,
            max_nodes_per_query: MAX_ACCESSIBILITY_QUERY_VISITED_NODES,
            max_selector_depth: MAX_ACCESSIBILITY_SELECTOR_DEPTH,
            max_query_matches: MAX_ACCESSIBILITY_QUERY_MATCHES,
            query_timeout_ms: MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS,
            max_snapshot_nodes: MAX_ACCESSIBILITY_SNAPSHOT_NODES,
            max_snapshot_bytes: MAX_ACCESSIBILITY_SNAPSHOT_BYTES,
        }
    }
}

/// Content-free indication that an ingest operation changed observable state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessibilityIngestEvent {
    pub(crate) desktop_id: DesktopId,
    pub(crate) desktop_generation: DesktopGeneration,
    pub(crate) kind: AccessibilityIngestKind,
    pub(crate) atspi_generation: AtspiGeneration,
    pub(crate) revision: AccessibilityRevision,
    pub(crate) cache_sequence: u64,
    pub(crate) sources: Vec<AccessibilityIngestSource>,
}

/// Secret-free source evidence captured under the same lock as one committed ingest.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessibilityIngestSource {
    pub(crate) raw: ObjectAddress,
    pub(crate) element: Option<ElementRef>,
    pub(crate) metadata: AccessibilityEventMetadata,
}

/// Bounded live metadata safe to place in an accessibility event.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AccessibilityEventMetadata {
    pub(crate) bounds: Option<Rect>,
    pub(crate) value: Option<f64>,
    pub(crate) caret_offset: Option<u32>,
    pub(crate) text_selection: Option<(u32, u32)>,
}

/// Current public cache coordinates plus best-effort resolution of one raw source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessibilitySourceResolution {
    pub(crate) desktop_id: DesktopId,
    pub(crate) desktop_generation: DesktopGeneration,
    pub(crate) atspi_generation: AtspiGeneration,
    pub(crate) revision: AccessibilityRevision,
    pub(crate) cache_sequence: u64,
    pub(crate) source: AccessibilityIngestSource,
}

/// Mutation class safe to log or publish without accessible content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessibilityIngestKind {
    BootstrapPending,
    Rebuilt,
    Upserted,
    Refreshed,
    Removed,
    ApplicationInvalidated,
    Unchanged,
    ResyncRequired,
    #[allow(
        dead_code,
        reason = "constructed when the correlation coordinator is wired"
    )]
    Correlated,
}

/// Content-free reason supplied by the actor/runtime resync lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessibilityResyncCause {
    #[allow(dead_code, reason = "used by actor-signal integration and tests")]
    ActorSignal,
    HealthGenerationChange,
    EventGap,
    EventQueueOverflow,
}

/// Opaque daemon proof used to request one actor-owned fresh semantic target.
///
/// It intentionally contains no proxy and no independently constructed AT-SPI
/// identity fingerprint. Every field is copied from one serialized mirror
/// revision and must be revalidated before actor submission.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "consumed by the semantic-action coordinator in the next integration slice"
)]
pub(crate) struct AccessibilityActionTargetEvidence {
    object: ObjectAddress,
    application: ObjectAddress,
    accessibility_generation: u64,
    application_generation: u64,
    source_revision: u64,
    cache_revision: AccessibilityRevision,
    node_revision: u64,
    current_element: ElementRef,
}

#[allow(
    dead_code,
    reason = "consumed by the semantic-action coordinator in the next integration slice"
)]
impl AccessibilityActionTargetEvidence {
    /// Exact central-bus object address.
    pub(crate) fn object(&self) -> &ObjectAddress {
        &self.object
    }

    /// Exact central-bus application root address.
    pub(crate) fn application(&self) -> &ObjectAddress {
        &self.application
    }

    /// Actor accessibility connection generation.
    pub(crate) const fn accessibility_generation(&self) -> u64 {
        self.accessibility_generation
    }

    /// Unique application-owner generation.
    pub(crate) const fn application_generation(&self) -> u64 {
        self.application_generation
    }

    /// Actor cache revision mirrored by the daemon.
    pub(crate) const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Daemon semantic-cache revision resolved atomically with this proof.
    pub(crate) const fn cache_revision(&self) -> AccessibilityRevision {
        self.cache_revision
    }

    /// Last actor mutation revision for the exact node.
    pub(crate) const fn node_revision(&self) -> u64 {
        self.node_revision
    }

    /// Exact currently minted public object birth.
    pub(crate) fn current_element(&self) -> &ElementRef {
        &self.current_element
    }

    /// Builds the bounded request whose secret identity fields only the actor can fill.
    pub(crate) fn semantic_target_request(&self) -> SemanticTargetRequest {
        SemanticTargetRequest {
            object: self.object.clone(),
            application: self.application.clone(),
            accessibility_generation: self.accessibility_generation,
            application_generation: self.application_generation,
            cache_revision: self.source_revision,
            node_revision: self.node_revision,
        }
    }

    /// Rejects a fresh actor result that does not name this exact cache proof.
    pub(crate) fn validate_observation(
        &self,
        fresh: &SemanticObservationResult,
    ) -> Result<(), AccessibilityPlaneError> {
        if fresh.accessibility_generation != self.accessibility_generation
            || fresh.application_generation != self.application_generation
            || fresh.cache_revision < self.source_revision
            || fresh.object != self.object
            || fresh.application != self.application
            || fresh.read_epoch == 0
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }
        Ok(())
    }
}

/// One exact application or top-level accessible offered to the correlation lane.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
pub(crate) struct AccessibilityCorrelationTarget {
    evidence: AccessibilityActionTargetEvidence,
    snapshot: ElementSnapshot,
    application_name: Option<String>,
}

#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
impl AccessibilityCorrelationTarget {
    /// Opaque exact actor/cache proof used for a fresh semantic observation.
    pub(crate) const fn evidence(&self) -> &AccessibilityActionTargetEvidence {
        &self.evidence
    }

    /// Bounded public projection at the same daemon cache revision as the proof.
    pub(crate) const fn snapshot(&self) -> &ElementSnapshot {
        &self.snapshot
    }

    /// Builds pure correlation input only after validating a fresh actor result.
    pub(crate) fn correlation_subject(
        &self,
        fresh: &SemanticObservationResult,
        observed_at: MonotonicMillis,
        explicit_window: Option<xenoteer_protocol::WindowRef>,
        profiled_root_extents: Option<Rect>,
        broker_verified_managed_process_id: Option<u32>,
    ) -> Result<AccessibilityCorrelationSubject, AccessibilityPlaneError> {
        self.evidence.validate_observation(fresh)?;
        if profiled_root_extents.is_some()
            && fresh.evidence.top_level.as_ref() != Some(&fresh.object)
        {
            return Err(AccessibilityPlaneError::InvalidRequest);
        }
        if broker_verified_managed_process_id.is_some()
            && broker_verified_managed_process_id != fresh.evidence.application_pid
        {
            return Err(AccessibilityPlaneError::InvalidRequest);
        }
        Ok(AccessibilityCorrelationSubject {
            application: self.snapshot.element.application.clone(),
            element: self.snapshot.element.clone(),
            process_id: fresh.evidence.application_pid,
            // The coordinator may promote the fresh application PID only after
            // matching it to processd-owned candidate evidence.
            managed_process_id: broker_verified_managed_process_id,
            // AT-SPI screen coordinates must not be relabeled as root-physical
            // without an explicit desktop-profile transform.
            top_level_extents: profiled_root_extents,
            title: self
                .snapshot
                .name
                .as_ref()
                .map(NormalizedCorrelationText::new)
                .transpose()
                .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
            // AT-SPI's application name is a human-facing label, whereas the
            // X11 candidate identity is WM_CLASS. They are different identity
            // namespaces (for example, "Xenoteer GTK3 Fixture" versus
            // "XenoteerFixture") and treating inequality as conflicting
            // evidence makes an otherwise exact caller/PID/geometry binding
            // fail closed. Leave this unset until a backend supplies a
            // namespace-compatible desktop/application identifier.
            application_identity: None,
            toolkit_identity: None,
            focused: raw_state_contains(&fresh.evidence.states, 12),
            focus_changed_at: None,
            created_at: None,
            observed_at,
            explicit_window,
            client_leader: None,
        })
    }

    /// Binds one pure policy result to the exact evidence that produced it.
    pub(crate) fn assignment(
        &self,
        correlation: ElementWindowCorrelation,
    ) -> AccessibilityCorrelationAssignment {
        AccessibilityCorrelationAssignment {
            evidence: self.evidence.clone(),
            correlation,
        }
    }
}

/// Common generation/revision fence shared by one target enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
pub(crate) struct AccessibilityCorrelationFence {
    accessibility_generation: u64,
    source_revision: u64,
    cache_revision: AccessibilityRevision,
}

/// Complete bounded application/top-level target universe from one cache revision.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
pub(crate) struct AccessibilityCorrelationTargets {
    fence: AccessibilityCorrelationFence,
    targets: Vec<AccessibilityCorrelationTarget>,
}

#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
impl AccessibilityCorrelationTargets {
    pub(crate) const fn fence(&self) -> AccessibilityCorrelationFence {
        self.fence
    }

    pub(crate) fn targets(&self) -> &[AccessibilityCorrelationTarget] {
        &self.targets
    }
}

/// Exact child proof and complete target universe captured under one plane lock.
#[derive(Clone, Debug)]
pub(crate) struct AccessibilityElementCorrelationContext {
    fence: AccessibilityCorrelationFence,
    element_evidence: AccessibilityActionTargetEvidence,
    targets: Vec<AccessibilityCorrelationTarget>,
}

/// One evidence-bound result admitted for atomic installation.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
pub(crate) struct AccessibilityCorrelationAssignment {
    evidence: AccessibilityActionTargetEvidence,
    correlation: ElementWindowCorrelation,
}

const DEFAULT_CORRELATION_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_CORRELATION_PASS_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_CORRELATION_STALE_RETRIES: usize = 1;
const MAX_CORRELATION_STALE_RETRIES: usize = 3;

/// Bounded cadence and pure-policy limits for the production correlation lane.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AccessibilityCorrelationCoordinatorConfig {
    pub(crate) interval: Duration,
    pub(crate) pass_timeout: Duration,
    pub(crate) max_stale_retries: usize,
    pub(crate) limits: AccessibilityCorrelationLimits,
}

impl Default for AccessibilityCorrelationCoordinatorConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_CORRELATION_INTERVAL,
            pass_timeout: DEFAULT_CORRELATION_PASS_TIMEOUT,
            max_stale_retries: DEFAULT_CORRELATION_STALE_RETRIES,
            limits: AccessibilityCorrelationLimits::default(),
        }
    }
}

impl AccessibilityCorrelationCoordinatorConfig {
    fn validate(self) -> Result<Self, AccessibilityCorrelationCoordinatorError> {
        if self.interval.is_zero()
            || self.pass_timeout.is_zero()
            || self.max_stale_retries > MAX_CORRELATION_STALE_RETRIES
        {
            return Err(AccessibilityCorrelationCoordinatorError::InvalidConfiguration);
        }
        self.limits
            .validate()
            .map_err(AccessibilityCorrelationCoordinatorError::Correlation)?;
        Ok(self)
    }
}

/// Stable failure classes for one bounded correlation attempt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AccessibilityCorrelationCoordinatorError {
    #[error("invalid accessibility correlation coordinator configuration")]
    InvalidConfiguration,
    #[error("accessibility correlation was cancelled")]
    Cancelled,
    #[error("accessibility correlation exhausted its bounded stale-evidence retries")]
    StaleEvidenceExhausted,
    #[error("accessibility plane rejected correlation evidence: {0:?}")]
    Plane(AccessibilityPlaneError),
    #[error(transparent)]
    Window(#[from] ControlPlaneError),
    #[error(transparent)]
    Actor(#[from] SemanticError),
    #[error(transparent)]
    Correlation(#[from] AccessibilityCorrelationError),
}

/// Content-free diagnostic category for errors that can wrap toolkit-owned
/// remote failure descriptions.
pub(crate) const fn accessibility_correlation_error_class(
    error: &AccessibilityCorrelationCoordinatorError,
) -> &'static str {
    match error {
        AccessibilityCorrelationCoordinatorError::InvalidConfiguration => "invalid_configuration",
        AccessibilityCorrelationCoordinatorError::Cancelled => "cancelled",
        AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted => {
            "stale_evidence_exhausted"
        }
        AccessibilityCorrelationCoordinatorError::Plane(_) => "accessibility_plane",
        AccessibilityCorrelationCoordinatorError::Window(_) => "window_plane",
        AccessibilityCorrelationCoordinatorError::Actor(_) => "atspi_actor",
        AccessibilityCorrelationCoordinatorError::Correlation(_) => "correlation",
    }
}

impl From<AccessibilityPlaneError> for AccessibilityCorrelationCoordinatorError {
    fn from(error: AccessibilityPlaneError) -> Self {
        Self::Plane(error)
    }
}

/// Actor-owned target and fresh observation returned from one serialized read.
#[derive(Clone, Debug)]
pub(crate) struct FreshAccessibilityCorrelationObservation {
    observation: SemanticObservationResult,
}

/// Injected exact-observation boundary used by both production and tests.
pub(crate) trait AccessibilityCorrelationObserver: Send + Sync + 'static {
    fn observe_exact<'a>(
        &'a self,
        evidence: &'a AccessibilityActionTargetEvidence,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AccessibilityFuture<
        'a,
        Result<FreshAccessibilityCorrelationObservation, AccessibilityCorrelationCoordinatorError>,
    >;
}

impl AccessibilityCorrelationObserver for AtspiHandle {
    fn observe_exact<'a>(
        &'a self,
        evidence: &'a AccessibilityActionTargetEvidence,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AccessibilityFuture<
        'a,
        Result<FreshAccessibilityCorrelationObservation, AccessibilityCorrelationCoordinatorError>,
    > {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AccessibilityCorrelationCoordinatorError::Cancelled);
            }
            let target = timeout_at(
                deadline,
                self.semantic_target(
                    evidence.semantic_target_request(),
                    cancellation.child_token(),
                ),
            )
            .await
            .map_err(|_| SemanticError::DeadlineBeforeDispatch)??;
            let observation = timeout_at(
                deadline,
                self.observe_semantic(
                    SemanticObservationRequest { target, deadline },
                    cancellation.child_token(),
                ),
            )
            .await
            .map_err(|_| SemanticError::DeadlineBeforeDispatch)??;
            Ok(FreshAccessibilityCorrelationObservation { observation })
        })
    }
}

/// Injected window-model boundary preserving a revision fence through commit.
pub(crate) trait AccessibilityCorrelationWindowSource: Send + Sync + 'static {
    fn snapshot<'a>(
        &'a self,
    ) -> AccessibilityFuture<
        'a,
        Result<ObservationCorrelationSnapshot, AccessibilityCorrelationCoordinatorError>,
    >;

    fn replace<'a>(
        &'a self,
        expected_revision: WindowModelRevision,
        windows: Vec<WindowRef>,
    ) -> AccessibilityFuture<
        'a,
        Result<WindowModelRevision, AccessibilityCorrelationCoordinatorError>,
    >;
}

impl AccessibilityCorrelationWindowSource for DaemonObservationService {
    fn snapshot<'a>(
        &'a self,
    ) -> AccessibilityFuture<
        'a,
        Result<ObservationCorrelationSnapshot, AccessibilityCorrelationCoordinatorError>,
    > {
        Box::pin(async move {
            self.accessibility_correlation_snapshot()
                .await
                .map_err(Into::into)
        })
    }

    fn replace<'a>(
        &'a self,
        expected_revision: WindowModelRevision,
        windows: Vec<WindowRef>,
    ) -> AccessibilityFuture<
        'a,
        Result<WindowModelRevision, AccessibilityCorrelationCoordinatorError>,
    > {
        Box::pin(async move {
            self.replace_accessibility_correlations(expected_revision, windows)
                .await
                .map_err(Into::into)
        })
    }
}

/// Content-free summary from one complete, atomically committed correlation pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccessibilityCorrelationPass {
    pub(crate) accessibility_revision: AccessibilityRevision,
    pub(crate) window_revision: WindowModelRevision,
    pub(crate) target_count: usize,
    pub(crate) correlated_window_count: usize,
}

/// One profile-owned conversion from exact AT-SPI screen bounds to root pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccessibilityProfiledRect {
    pub(crate) atspi_screen: SemanticRect,
    pub(crate) root_physical: Rect,
}

/// Exact on-demand correlation evidence retained for queue-head re-observation.
#[derive(Clone, Debug)]
pub(crate) struct AccessibilityExplicitCorrelationEvidence {
    element_evidence: AccessibilityActionTargetEvidence,
    correlation_target: AccessibilityCorrelationTarget,
    element_observation: SemanticObservationResult,
    correlation_observation: SemanticObservationResult,
    window_snapshot: ObservationCorrelationSnapshot,
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "retained for evidence audit tests")
    )]
    correlation: ElementWindowCorrelation,
    explicit_window: Option<WindowRef>,
    fence: AccessibilityCorrelationFence,
    window_revision: WindowModelRevision,
}

/// Production coordinator for background and caller-explicit correlation.
pub(crate) struct AccessibilityCorrelationCoordinator {
    plane: Arc<DaemonAccessibilityPlane>,
    observer: Arc<dyn AccessibilityCorrelationObserver>,
    windows: Arc<dyn AccessibilityCorrelationWindowSource>,
    config: AccessibilityCorrelationCoordinatorConfig,
}

impl fmt::Debug for AccessibilityCorrelationCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessibilityCorrelationCoordinator")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AccessibilityCorrelationCoordinator {
    pub(crate) fn new(
        plane: Arc<DaemonAccessibilityPlane>,
        observer: Arc<dyn AccessibilityCorrelationObserver>,
        windows: Arc<dyn AccessibilityCorrelationWindowSource>,
        config: AccessibilityCorrelationCoordinatorConfig,
    ) -> Result<Self, AccessibilityCorrelationCoordinatorError> {
        Ok(Self {
            plane,
            observer,
            windows,
            config: config.validate()?,
        })
    }

    pub(crate) fn live(
        plane: Arc<DaemonAccessibilityPlane>,
        actor: AtspiHandle,
        windows: Arc<DaemonObservationService>,
        config: AccessibilityCorrelationCoordinatorConfig,
    ) -> Result<Self, AccessibilityCorrelationCoordinatorError> {
        Self::new(plane, Arc::new(actor), windows, config)
    }

    /// Runs immediately, then at a fixed skipped-tick cadence until cancelled.
    pub(crate) fn spawn(self: Arc<Self>, cancellation: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut cadence = interval(self.config.interval);
            cadence.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = cadence.tick() => {
                        if let Err(error) = self.reconcile_once(cancellation.child_token()).await {
                            // Stale correlations are unsafe to retain after a failed fresh pass.
                            self.clear_outputs_best_effort().await;
                            tracing::debug!(
                                error_class = accessibility_correlation_error_class(&error),
                                "accessibility correlation pass failed closed"
                            );
                        }
                    }
                }
            }
            self.clear_outputs_best_effort().await;
        })
    }

    /// Completes one all-target pass, retrying only exact-fence drift.
    pub(crate) async fn reconcile_once(
        &self,
        cancellation: CancellationToken,
    ) -> Result<AccessibilityCorrelationPass, AccessibilityCorrelationCoordinatorError> {
        let deadline = Instant::now() + self.config.pass_timeout;
        for attempt in 0..=self.config.max_stale_retries {
            if cancellation.is_cancelled() {
                return Err(AccessibilityCorrelationCoordinatorError::Cancelled);
            }
            match self
                .reconcile_attempt(deadline, cancellation.child_token())
                .await
            {
                Err(error)
                    if correlation_error_is_stale(&error)
                        && attempt < self.config.max_stale_retries =>
                {
                    continue;
                }
                Err(error) if correlation_error_is_stale(&error) => {
                    return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
                }
                result => return result,
            }
        }
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    }

    async fn reconcile_attempt(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<AccessibilityCorrelationPass, AccessibilityCorrelationCoordinatorError> {
        require_correlation_time(deadline, &cancellation)?;
        let targets = timeout_at(deadline, self.plane.accessibility_correlation_targets())
            .await
            .map_err(|_| correlation_deadline_error())??;
        let mut fresh = Vec::with_capacity(targets.targets().len());
        for target in targets.targets() {
            require_correlation_time(deadline, &cancellation)?;
            match self
                .observer
                .observe_exact(target.evidence(), deadline, cancellation.child_token())
                .await
            {
                Ok(observation) => fresh.push(Some(observation)),
                Err(error) if correlation_error_is_application_local(&error) => fresh.push(None),
                Err(error) => return Err(error),
            }
        }
        require_correlation_time(deadline, &cancellation)?;
        let window_snapshot = timeout_at(deadline, self.windows.snapshot())
            .await
            .map_err(|_| correlation_deadline_error())??;
        let candidates = window_snapshot.candidates()?;
        let mut assignments = Vec::with_capacity(targets.targets().len());
        let mut correlated_windows = Vec::new();
        for (target, fresh) in targets.targets().iter().zip(&fresh) {
            let correlation = match fresh {
                None => empty_window_correlation(),
                Some(fresh) => {
                    let managed_pid = broker_verified_managed_pid(&fresh.observation, &candidates);
                    let subject = match target.correlation_subject(
                        &fresh.observation,
                        window_snapshot.observed_at,
                        None,
                        None,
                        managed_pid,
                    ) {
                        Ok(subject) => subject,
                        Err(AccessibilityPlaneError::InvalidRequest) => {
                            assignments.push(target.assignment(empty_window_correlation()));
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    correlate_accessibility_window(
                        &subject,
                        &candidates,
                        window_snapshot.observed_at,
                        self.config.limits,
                    )?
                }
            };
            if let Some(window) = &correlation.window
                && !correlated_windows.contains(window)
            {
                correlated_windows.push(window.clone());
            }
            assignments.push(target.assignment(correlation));
        }
        require_correlation_time(deadline, &cancellation)?;
        let accessibility = timeout_at(
            deadline,
            self.plane
                .replace_window_correlations(targets.fence(), assignments),
        )
        .await
        .map_err(|_| correlation_deadline_error())??;
        let window_revision = match timeout_at(
            deadline,
            self.windows
                .replace(window_snapshot.revision, correlated_windows.clone()),
        )
        .await
        {
            Ok(Ok(revision)) => revision,
            Ok(Err(error)) => {
                self.clear_outputs_best_effort().await;
                return Err(error);
            }
            Err(_) => {
                self.clear_outputs_best_effort().await;
                return Err(correlation_deadline_error());
            }
        };
        Ok(AccessibilityCorrelationPass {
            accessibility_revision: accessibility.revision,
            window_revision,
            target_count: targets.targets().len(),
            correlated_window_count: correlated_windows.len(),
        })
    }

    /// Correlates one exact child, optionally constraining the result to a
    /// caller-supplied exact X11 birth. `None` adds no explicit-reference
    /// signal and therefore cannot promote derived evidence.
    ///
    /// The child is observed first. Its fresh actor-owned `top_level` address
    /// then selects an exact member of the complete target set captured under
    /// the same daemon fence.
    pub(crate) async fn correlate_element(
        &self,
        element: &ElementRef,
        explicit_window: Option<WindowRef>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<AccessibilityExplicitCorrelationEvidence, AccessibilityCorrelationCoordinatorError>
    {
        for attempt in 0..=self.config.max_stale_retries {
            if cancellation.is_cancelled() {
                return Err(AccessibilityCorrelationCoordinatorError::Cancelled);
            }
            match self
                .correlate_explicit_attempt(
                    element,
                    explicit_window.clone(),
                    deadline,
                    cancellation.child_token(),
                )
                .await
            {
                Err(error)
                    if correlation_error_is_stale(&error)
                        && attempt < self.config.max_stale_retries =>
                {
                    tracing::debug!(
                        attempt,
                        error_class = accessibility_correlation_error_class(&error),
                        "explicit accessibility correlation retrying stale evidence"
                    );
                    continue;
                }
                Err(error) if correlation_error_is_stale(&error) => {
                    tracing::debug!(
                        attempt,
                        error_class = accessibility_correlation_error_class(&error),
                        "explicit accessibility correlation exhausted stale evidence"
                    );
                    return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
                }
                result => return result,
            }
        }
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    }

    async fn correlate_explicit_attempt(
        &self,
        element: &ElementRef,
        explicit_window: Option<WindowRef>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<AccessibilityExplicitCorrelationEvidence, AccessibilityCorrelationCoordinatorError>
    {
        require_correlation_time(deadline, &cancellation)?;
        let context = timeout_at(
            deadline,
            self.plane
                .accessibility_element_correlation_context(element),
        )
        .await
        .map_err(|_| correlation_deadline_error())?
        .map_err(|error| {
            tracing::debug!("explicit accessibility correlation context rejected");
            AccessibilityCorrelationCoordinatorError::Plane(error)
        })?;
        let element_fresh = self
            .observer
            .observe_exact(
                &context.element_evidence,
                deadline,
                cancellation.child_token(),
            )
            .await?;
        context
            .element_evidence
            .validate_observation(&element_fresh.observation)
            .map_err(|error| {
                tracing::debug!("explicit accessibility child observation rejected");
                AccessibilityCorrelationCoordinatorError::Plane(error)
            })?;
        let top_level = element_fresh
            .observation
            .evidence
            .top_level
            .as_ref()
            .ok_or(AccessibilityPlaneError::UnsupportedByTarget)?;
        let correlation_target = context
            .targets
            .iter()
            .find(|target| target.evidence().object() == top_level)
            .cloned()
            .ok_or_else(|| {
                tracing::debug!(
                    "explicit accessibility top-level was absent from the fenced target universe"
                );
                AccessibilityPlaneError::StaleReference {
                    current_generation: None,
                }
            })?;
        let correlation_fresh = if correlation_target.evidence() == &context.element_evidence {
            element_fresh.clone()
        } else {
            self.observer
                .observe_exact(
                    correlation_target.evidence(),
                    deadline,
                    cancellation.child_token(),
                )
                .await?
        };
        require_correlation_time(deadline, &cancellation)?;
        let window_snapshot = timeout_at(deadline, self.windows.snapshot())
            .await
            .map_err(|_| correlation_deadline_error())??;
        require_correlation_time(deadline, &cancellation)?;
        let candidates = window_snapshot.candidates()?;
        let managed_pid = broker_verified_managed_pid(&correlation_fresh.observation, &candidates);
        let subject = correlation_target
            .correlation_subject(
                &correlation_fresh.observation,
                window_snapshot.observed_at,
                explicit_window.clone(),
                None,
                managed_pid,
            )
            .map_err(|error| {
                tracing::debug!("explicit accessibility top-level observation rejected");
                AccessibilityCorrelationCoordinatorError::Plane(error)
            })?;
        let correlation = correlate_accessibility_window(
            &subject,
            &candidates,
            window_snapshot.observed_at,
            self.config.limits,
        )?;
        let window_revision = window_snapshot.revision;
        Ok(AccessibilityExplicitCorrelationEvidence {
            element_evidence: context.element_evidence,
            correlation_target,
            element_observation: element_fresh.observation,
            correlation_observation: correlation_fresh.observation,
            window_snapshot,
            correlation,
            explicit_window,
            fence: context.fence,
            window_revision,
        })
    }

    async fn clear_outputs_best_effort(&self) {
        let deadline = Instant::now() + self.config.pass_timeout;
        if let Ok(Ok(targets)) =
            timeout_at(deadline, self.plane.accessibility_correlation_targets()).await
        {
            let _ = timeout_at(
                deadline,
                self.plane
                    .replace_window_correlations(targets.fence(), Vec::new()),
            )
            .await;
        }
        if let Ok(Ok(snapshot)) = timeout_at(deadline, self.windows.snapshot()).await {
            let _ = timeout_at(
                deadline,
                self.windows.replace(snapshot.revision, Vec::new()),
            )
            .await;
        }
    }
}

impl AccessibilityExplicitCorrelationEvidence {
    pub(crate) fn admission_element_observation(&self) -> &SemanticObservationResult {
        &self.element_observation
    }

    pub(crate) fn admission_correlation_observation(&self) -> &SemanticObservationResult {
        &self.correlation_observation
    }

    #[cfg(test)]
    pub(crate) fn admission_window_snapshot(&self) -> &ObservationCorrelationSnapshot {
        &self.window_snapshot
    }

    #[cfg(test)]
    pub(crate) fn correlation(&self) -> &ElementWindowCorrelation {
        &self.correlation
    }

    /// Replaces the admission observations with one coherent queue-head read
    /// and recomputes the explicit-window correlation from that fresh universe.
    #[cfg(test)]
    pub(crate) fn with_fresh_observations(
        &self,
        element_fresh: SemanticObservationResult,
        correlation_fresh: SemanticObservationResult,
        windows: ObservationCorrelationSnapshot,
        limits: AccessibilityCorrelationLimits,
    ) -> Result<Self, AccessibilityCorrelationCoordinatorError> {
        self.element_evidence.validate_observation(&element_fresh)?;
        self.correlation_target
            .evidence()
            .validate_observation(&correlation_fresh)?;
        require_coherent_observation_pair(
            &self.element_evidence,
            &self.correlation_target.evidence,
            &element_fresh,
            &correlation_fresh,
        )?;
        if windows.revision.get() < self.window_revision.get() {
            return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
        }
        let candidates = windows.candidates()?;
        let managed_pid = broker_verified_managed_pid(&correlation_fresh, &candidates);
        let subject = self.correlation_target.correlation_subject(
            &correlation_fresh,
            windows.observed_at,
            self.explicit_window.clone(),
            None,
            managed_pid,
        )?;
        let correlation =
            correlate_accessibility_window(&subject, &candidates, windows.observed_at, limits)?;
        let mut refreshed = self.clone();
        refreshed.element_observation = element_fresh;
        refreshed.correlation_observation = correlation_fresh;
        refreshed.window_revision = windows.revision;
        refreshed.window_snapshot = windows;
        refreshed.correlation = correlation;
        Ok(refreshed)
    }

    /// Re-mints the exact current public births and observes them as one
    /// queue-head universe before any physical input effect is allowed.
    ///
    /// The daemon plane is sampled first without waiting for its lock. Exact
    /// actor target minting then proves the actor has reached precisely that
    /// mirror revision. Paired observations must come from the same actor
    /// cache revision, so a child or top-level mutation between reads fails
    /// closed instead of composing geometry from two different universes.
    pub(crate) fn refresh_for_queue_head_blocking(
        &self,
        plane: &DaemonAccessibilityPlane,
        actor: &AtspiHandle,
        windows: &DaemonObservationService,
        deadline: Instant,
        limits: AccessibilityCorrelationLimits,
    ) -> Result<Self, AccessibilityCorrelationCoordinatorError> {
        let context = plane.accessibility_element_correlation_context_blocking(
            self.element_evidence.current_element(),
        )?;
        if context.fence.accessibility_generation != self.fence.accessibility_generation
            || context.fence.source_revision < self.fence.source_revision
            || context.fence.cache_revision < self.fence.cache_revision
        {
            return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
        }

        let element_semantic_target = actor.semantic_target_blocking(
            context.element_evidence.semantic_target_request(),
            remaining_blocking_correlation_time(deadline)?,
        )?;
        let element_observation = actor.observe_semantic_blocking(
            SemanticObservationRequest {
                target: element_semantic_target,
                deadline,
            },
            remaining_blocking_correlation_time(deadline)?,
        )?;
        context
            .element_evidence
            .validate_observation(&element_observation)?;

        let top_level = element_observation
            .evidence
            .top_level
            .as_ref()
            .ok_or(AccessibilityPlaneError::UnsupportedByTarget)?;
        let correlation_target = context
            .targets
            .iter()
            .find(|target| target.evidence().object() == top_level)
            .cloned()
            .ok_or(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            })?;
        let correlation_observation = if correlation_target.evidence() == &context.element_evidence
        {
            element_observation.clone()
        } else {
            let target = actor.semantic_target_blocking(
                correlation_target.evidence().semantic_target_request(),
                remaining_blocking_correlation_time(deadline)?,
            )?;
            actor.observe_semantic_blocking(
                SemanticObservationRequest { target, deadline },
                remaining_blocking_correlation_time(deadline)?,
            )?
        };
        correlation_target
            .evidence()
            .validate_observation(&correlation_observation)?;
        require_coherent_observation_pair(
            &context.element_evidence,
            correlation_target.evidence(),
            &element_observation,
            &correlation_observation,
        )?;

        let window_snapshot = windows.accessibility_correlation_snapshot_blocking(
            remaining_blocking_correlation_time(deadline)?,
        )?;
        if window_snapshot.revision.get() < self.window_revision.get() {
            return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
        }
        let candidates = window_snapshot.candidates()?;
        let managed_pid = broker_verified_managed_pid(&correlation_observation, &candidates);
        let subject = correlation_target.correlation_subject(
            &correlation_observation,
            window_snapshot.observed_at,
            self.explicit_window.clone(),
            None,
            managed_pid,
        )?;
        let correlation = correlate_accessibility_window(
            &subject,
            &candidates,
            window_snapshot.observed_at,
            limits,
        )?;
        let window_revision = window_snapshot.revision;
        Ok(Self {
            element_evidence: context.element_evidence,
            correlation_target,
            element_observation,
            correlation_observation,
            window_snapshot,
            correlation,
            explicit_window: self.explicit_window.clone(),
            fence: context.fence,
            window_revision,
        })
    }

    /// Recomputes correlation from fresh actor and X11 evidence, then projects
    /// a click observation only through profile-bound coordinate conversions.
    pub(crate) fn click_observation(
        &self,
        element_extents: AccessibilityProfiledRect,
        top_level_extents: Option<AccessibilityProfiledRect>,
        root_bounds: Rect,
        limits: AccessibilityCorrelationLimits,
    ) -> Result<ElementClickObservation, AccessibilityCorrelationCoordinatorError> {
        let element_fresh = &self.element_observation;
        let correlation_fresh = &self.correlation_observation;
        let windows = &self.window_snapshot;
        self.element_evidence.validate_observation(element_fresh)?;
        self.correlation_target
            .evidence()
            .validate_observation(correlation_fresh)?;
        if element_fresh.evidence.bounds != Some(element_extents.atspi_screen)
            || top_level_extents.is_some_and(|profiled| {
                correlation_fresh.evidence.bounds != Some(profiled.atspi_screen)
            })
        {
            return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
        }
        if windows.revision.get() < self.window_revision.get() {
            return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
        }
        let candidates = windows.candidates()?;
        let managed_pid = broker_verified_managed_pid(correlation_fresh, &candidates);
        let subject = self.correlation_target.correlation_subject(
            correlation_fresh,
            windows.observed_at,
            self.explicit_window.clone(),
            top_level_extents.map(|profiled| profiled.root_physical),
            managed_pid,
        )?;
        let correlation =
            correlate_accessibility_window(&subject, &candidates, windows.observed_at, limits)?;
        let correlated_client_bounds = correlation.window.as_ref().and_then(|window| {
            candidates
                .iter()
                .find(|candidate| candidate.window == *window)
                .and_then(|candidate| candidate.top_level_extents)
        });
        Ok(ElementClickObservation {
            element: self.element_evidence.current_element().clone(),
            revision: AccessibilityRevision::new(element_fresh.cache_revision)
                .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
            read_epoch: element_fresh.read_epoch,
            element_extents: element_extents.root_physical,
            root_bounds,
            correlated_client_bounds,
            correlation,
        })
    }
}

fn require_coherent_observation_pair(
    element: &AccessibilityActionTargetEvidence,
    correlation: &AccessibilityActionTargetEvidence,
    element_fresh: &SemanticObservationResult,
    correlation_fresh: &SemanticObservationResult,
) -> Result<(), AccessibilityCorrelationCoordinatorError> {
    let coherent_epoch = if element == correlation {
        element_fresh.read_epoch == correlation_fresh.read_epoch
    } else {
        correlation_fresh.read_epoch > element_fresh.read_epoch
    };
    if element_fresh.cache_revision != correlation_fresh.cache_revision || !coherent_epoch {
        return Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted);
    }
    Ok(())
}

fn remaining_blocking_correlation_time(
    deadline: Instant,
) -> Result<Duration, AccessibilityCorrelationCoordinatorError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(correlation_deadline_error())
    } else {
        Ok(remaining)
    }
}

fn broker_verified_managed_pid(
    fresh: &SemanticObservationResult,
    candidates: &[AccessibilityWindowCandidate],
) -> Option<u32> {
    fresh.evidence.application_pid.filter(|pid| {
        candidates
            .iter()
            .any(|candidate| candidate.live && candidate.managed_process_id == Some(*pid))
    })
}

fn empty_window_correlation() -> ElementWindowCorrelation {
    ElementWindowCorrelation {
        window: None,
        confidence: WindowCorrelationConfidence::None,
        evidence: Vec::new(),
        conflicting_evidence: false,
    }
}

fn require_correlation_time(
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), AccessibilityCorrelationCoordinatorError> {
    if cancellation.is_cancelled() {
        Err(AccessibilityCorrelationCoordinatorError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(correlation_deadline_error())
    } else {
        Ok(())
    }
}

fn correlation_deadline_error() -> AccessibilityCorrelationCoordinatorError {
    AccessibilityCorrelationCoordinatorError::Actor(SemanticError::DeadlineBeforeDispatch)
}

fn correlation_error_is_stale(error: &AccessibilityCorrelationCoordinatorError) -> bool {
    matches!(
        error,
        AccessibilityCorrelationCoordinatorError::Plane(
            AccessibilityPlaneError::NotFound
                | AccessibilityPlaneError::StaleReference { .. }
                | AccessibilityPlaneError::ResyncRequired { .. },
        ) | AccessibilityCorrelationCoordinatorError::Window(
            ControlPlaneError::NotFound | ControlPlaneError::StaleReference { .. },
        ) | AccessibilityCorrelationCoordinatorError::Actor(
            SemanticError::StaleAccessibilityGeneration { .. }
                | SemanticError::StaleApplicationGeneration { .. }
                | SemanticError::StaleCacheRevision { .. }
                | SemanticError::StaleIdentity,
        )
    )
}

fn correlation_error_is_application_local(
    error: &AccessibilityCorrelationCoordinatorError,
) -> bool {
    matches!(
        error,
        AccessibilityCorrelationCoordinatorError::Actor(
            SemanticError::InterfaceUnavailable(_)
                | SemanticError::ActionNotFound
                | SemanticError::AmbiguousAction
                | SemanticError::UnclassifiedTextDenied
                | SemanticError::Backend(xenoteer_atspi::BackendFailure {
                    kind: xenoteer_atspi::BackendFailureKind::Protocol,
                    ..
                })
        )
    )
}

/// Content-free proof that one exact actor refresh was admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccessibilityPollDispatch {
    /// Actor accessibility generation at refresh admission.
    pub(crate) accessibility_generation: u64,
    /// Exact actor cache revision named by the refresh request.
    pub(crate) source_revision: u64,
    /// Last actor mutation revision of the target named by the refresh request.
    pub(crate) node_revision: u64,
}

/// Runtime-injected exact-reference refresh seam.
pub(crate) trait AccessibilityPollReconciler: Send + Sync + 'static {
    /// Refreshes one exact actor-fenced target without holding any plane lock.
    fn reconcile_exact<'a>(
        &'a self,
        evidence: AccessibilityActionTargetEvidence,
        deadline: Instant,
    ) -> AccessibilityFuture<'a, Result<AccessibilityPollDispatch, AccessibilityPlaneError>>;
}

#[derive(Clone, Copy, Debug)]
struct AccessibilityPollPolicy {
    initial_backoff: Duration,
    maximum_backoff: Duration,
    max_attempts: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccessibilityPollAttempt {
    Reconciled,
    DeadlineElapsed { dispatched: bool },
}

impl Default for AccessibilityPollPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: DEFAULT_POLL_INITIAL_BACKOFF,
            maximum_backoff: DEFAULT_POLL_MAXIMUM_BACKOFF,
            max_attempts: DEFAULT_POLL_MAX_ATTEMPTS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(
    dead_code,
    reason = "watch payload preserves generation/revision diagnostics for future consumers"
)]
struct ChangeSignal {
    atspi_generation: AtspiGeneration,
    revision: AccessibilityRevision,
}

struct BootstrapAccumulator {
    accessibility_generation: u64,
    source_revision: u64,
    nodes: BTreeMap<ObjectAddress, CachedNode>,
    estimated_bytes: usize,
    expected_after: Option<ObjectAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorRequestKind {
    List,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorBinding {
    digest: [u8; 32],
    kind: CursorRequestKind,
    order: ElementOrder,
    limit: u16,
    expansion: ElementSnapshotExpansion,
    limits: AccessibilityQueryLimits,
}

struct CursorRecord {
    principal: String,
    expires_at: Instant,
    binding: CursorBinding,
    continuation: AccessibilityContinuationDescriptor,
}

#[derive(Clone, Debug, PartialEq)]
struct InstalledWindowCorrelation {
    evidence: AccessibilityActionTargetEvidence,
    correlation: ElementWindowCorrelation,
}

struct PlaneState {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    cache: AccessibilityCache,
    source_revision: Option<u64>,
    has_bootstrapped: bool,
    expected_actor_generation: Option<u64>,
    resync_pending: bool,
    bootstrap: Option<BootstrapAccumulator>,
    raw_nodes: BTreeMap<ObjectAddress, CachedNode>,
    raw_estimated_bytes: usize,
    applications: BTreeMap<ObjectAddress, ApplicationRef>,
    actor_application_generations: BTreeMap<String, u64>,
    elements: BTreeMap<ObjectAddress, ElementRef>,
    correlations: BTreeMap<ObjectAddress, InstalledWindowCorrelation>,
    cursors: HashMap<[u8; 32], CursorRecord>,
}

/// Daemon implementation of the server accessibility boundary.
pub(crate) struct DaemonAccessibilityPlane {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    config: AccessibilityPlaneConfig,
    state: Mutex<PlaneState>,
    changes: watch::Sender<ChangeSignal>,
    wait_slots: Arc<Semaphore>,
    read_slots: Arc<Semaphore>,
    poll_slots: Arc<Semaphore>,
    poll_reconciler: std::sync::RwLock<Option<Arc<dyn AccessibilityPollReconciler>>>,
    poll_policy: std::sync::RwLock<AccessibilityPollPolicy>,
    #[cfg(test)]
    read_test_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl fmt::Debug for DaemonAccessibilityPlane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonAccessibilityPlane")
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .field("max_pending_waits", &self.config.max_pending_waits)
            .field("available_wait_slots", &self.wait_slots.available_permits())
            .field("max_pending_reads", &self.config.max_pending_reads)
            .field("available_read_slots", &self.read_slots.available_permits())
            .field("available_poll_slots", &self.poll_slots.available_permits())
            .field(
                "poll_reconciler_configured",
                &self
                    .poll_reconciler
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl DaemonAccessibilityPlane {
    pub(crate) fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        atspi_generation: AtspiGeneration,
        config: AccessibilityPlaneConfig,
    ) -> Result<Self, AccessibilityPlaneError> {
        if config.model_limits.validate().is_err()
            || config.raw_cache_limits.validate().is_err()
            || config.cursor_ttl.is_zero()
            || config.cursor_ttl > Duration::from_millis(u64::from(ACCESSIBILITY_CURSOR_TTL_MS))
            || config.max_total_cursors == 0
            || config.max_cursors_per_principal == 0
            || config.max_cursors_per_principal > config.max_total_cursors
            || config.max_pending_waits == 0
            || config.max_pending_reads == 0
            || config.max_bootstrap_bytes == 0
            || config.max_nodes_per_query == 0
            || config.max_nodes_per_query > MAX_ACCESSIBILITY_QUERY_VISITED_NODES
            || config.max_selector_depth == 0
            || config.max_selector_depth > MAX_ACCESSIBILITY_SELECTOR_DEPTH
            || config.max_query_matches == 0
            || config.max_query_matches > MAX_ACCESSIBILITY_QUERY_MATCHES
            || config.query_timeout_ms == 0
            || config.query_timeout_ms > MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS
            || config.max_snapshot_nodes == 0
            || config.max_snapshot_nodes > MAX_ACCESSIBILITY_SNAPSHOT_NODES
            || config.max_snapshot_nodes > config.max_nodes_per_query
            || config.max_snapshot_bytes == 0
            || config.max_snapshot_bytes > MAX_ACCESSIBILITY_SNAPSHOT_BYTES
        {
            return Err(AccessibilityPlaneError::InvalidRequest);
        }
        let cache = AccessibilityCache::new(
            desktop_id,
            desktop_generation,
            atspi_generation,
            config.model_limits,
        )
        .map_err(map_model_error)?;
        let signal = ChangeSignal {
            atspi_generation,
            revision: cache.revision(),
        };
        let (changes, _) = watch::channel(signal);
        Ok(Self {
            desktop_id,
            desktop_generation,
            config,
            state: Mutex::new(PlaneState {
                desktop_id,
                desktop_generation,
                cache,
                source_revision: None,
                has_bootstrapped: false,
                expected_actor_generation: Some(atspi_generation.get()),
                // Reads and incrementals are fail-closed until one complete,
                // authoritative actor bootstrap has been installed.
                resync_pending: true,
                bootstrap: None,
                raw_nodes: BTreeMap::new(),
                raw_estimated_bytes: 0,
                applications: BTreeMap::new(),
                actor_application_generations: BTreeMap::new(),
                elements: BTreeMap::new(),
                correlations: BTreeMap::new(),
                cursors: HashMap::new(),
            }),
            changes,
            wait_slots: Arc::new(Semaphore::new(config.max_pending_waits)),
            read_slots: Arc::new(Semaphore::new(config.max_pending_reads)),
            poll_slots: Arc::new(Semaphore::new(DEFAULT_MAX_PENDING_POLLS)),
            poll_reconciler: std::sync::RwLock::new(None),
            poll_policy: std::sync::RwLock::new(AccessibilityPollPolicy::default()),
            #[cfg(test)]
            read_test_hook: std::sync::Mutex::new(None),
        })
    }

    pub(crate) async fn list_for(
        &self,
        principal: &str,
        mut request: ElementListRequest,
    ) -> Result<ElementListPage, AccessibilityPlaneError> {
        request
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        self.require_desktop(request.desktop_id, request.desktop_generation)?;
        require_supported_expansion(request.expansion)?;
        request.limits = self.effective_query_limits(request.limits)?;
        request.limit = self.effective_page_limit(request.limit, request.limits);
        let expansion = request.expansion;
        let binding = list_cursor_binding(&request)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(request.limits.timeout_ms)
            .map_err(map_query_error)?;
        let permit = self.acquire_read_permit()?;
        let selector = ElementSelector {
            scope: request.scope.clone(),
            predicates: Vec::new(),
            order: request.order,
            result_index: None,
        };
        let (view, continuation) = {
            let mut state = self.state.lock().await;
            require_ready(&state)?;
            let continuation = request
                .cursor
                .as_ref()
                .map(|cursor| consume_cursor(&mut state, principal, cursor, &binding))
                .transpose()?;
            (state.cache.snapshot(), continuation)
        };
        let scope = request.scope.clone();
        let projection = self
            .run_admitted_read(permit, move || match continuation {
                None => view.list_with_deadline(
                    &scope,
                    request.order,
                    request.expansion,
                    request.limits,
                    request.limit,
                    0,
                    deadline,
                ),
                Some(continuation) => view.continue_query_with_deadline(
                    &selector,
                    request.limits,
                    request.limit,
                    &continuation,
                    deadline,
                ),
            })
            .await?
            .map_err(map_query_error)?;
        require_projected_expansion_evidence(&projection.elements, expansion)?;
        let mut state = self.state.lock().await;
        require_projection_current(&state, &projection)?;
        let next_cursor = issue_cursor(&mut state, principal, binding, &projection, self.config)?;
        let page = projection_to_page(projection, binding.order, next_cursor);
        let encoded_bytes = match serde_json::to_vec(&page) {
            Ok(encoded) => encoded.len(),
            Err(_) => {
                discard_page_cursor(&mut state, page.next_cursor.as_ref());
                return Err(AccessibilityPlaneError::Internal);
            }
        };
        if let Err(error) = self.require_emitted_snapshot_bounds(page.elements.len(), encoded_bytes)
        {
            discard_page_cursor(&mut state, page.next_cursor.as_ref());
            return Err(error);
        }
        Ok(page)
    }

    pub(crate) async fn query_for(
        &self,
        principal: &str,
        mut request: ElementQueryRequest,
    ) -> Result<ElementQueryPage, AccessibilityPlaneError> {
        request
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        self.require_desktop(request.desktop_id, request.desktop_generation)?;
        require_supported_expansion(request.expansion)?;
        request.limits = self.effective_query_limits(request.limits)?;
        request.limit = self.effective_page_limit(request.limit, request.limits);
        let expansion = request.expansion;
        let binding = query_cursor_binding(&request)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(request.limits.timeout_ms)
            .map_err(map_query_error)?;
        let permit = self.acquire_read_permit()?;
        let (view, continuation) = {
            let mut state = self.state.lock().await;
            require_ready(&state)?;
            require_selector_evidence(&state, &request.selector)?;
            let continuation = request
                .cursor
                .as_ref()
                .map(|cursor| consume_cursor(&mut state, principal, cursor, &binding))
                .transpose()?;
            (state.cache.snapshot(), continuation)
        };
        let projection = self
            .run_admitted_read(permit, move || match continuation {
                None => view.query_with_deadline(
                    &request.selector,
                    request.expansion,
                    request.limits,
                    request.limit,
                    0,
                    deadline,
                ),
                Some(continuation) => view.continue_query_with_deadline(
                    &request.selector,
                    request.limits,
                    request.limit,
                    &continuation,
                    deadline,
                ),
            })
            .await?
            .map_err(map_query_error)?;
        require_projected_expansion_evidence(&projection.elements, expansion)?;
        let mut state = self.state.lock().await;
        require_projection_current(&state, &projection)?;
        let next_cursor = issue_cursor(&mut state, principal, binding, &projection, self.config)?;
        let page = projection_to_page(projection, binding.order, next_cursor);
        let encoded_bytes = match serde_json::to_vec(&page) {
            Ok(encoded) => encoded.len(),
            Err(_) => {
                discard_page_cursor(&mut state, page.next_cursor.as_ref());
                return Err(AccessibilityPlaneError::Internal);
            }
        };
        if let Err(error) = self.require_emitted_snapshot_bounds(page.elements.len(), encoded_bytes)
        {
            discard_page_cursor(&mut state, page.next_cursor.as_ref());
            return Err(error);
        }
        Ok(page)
    }

    pub(crate) async fn resolve_for(
        &self,
        mut request: ElementResolveRequest,
    ) -> Result<ElementResolveResult, AccessibilityPlaneError> {
        request
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        self.require_desktop(request.desktop_id, request.desktop_generation)?;
        require_supported_expansion(request.expansion)?;
        request.limits = self.effective_query_limits(request.limits)?;
        let expansion = request.expansion;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(request.limits.timeout_ms)
            .map_err(map_query_error)?;
        let permit = self.acquire_read_permit()?;
        let view = {
            let state = self.state.lock().await;
            require_ready(&state)?;
            require_selector_evidence(&state, &request.selector)?;
            state.cache.snapshot()
        };
        let revision = view.revision();
        let mut element = self
            .run_admitted_read(permit, move || {
                view.resolve_exactly_one_with_deadline(
                    &request.selector,
                    request.expansion,
                    request.limits,
                    deadline,
                )
            })
            .await?
            .map_err(map_query_error)?;
        require_projected_expansion_evidence(std::slice::from_ref(&element), expansion)?;
        let state = self.state.lock().await;
        require_revision_current(&state, revision)?;
        // Exact-one represents the atomic resolution point, not the object's
        // last mutation revision.
        element.snapshot.revision = revision;
        let result = ElementResolveResult {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: state.cache.atspi_generation(),
            snapshot_revision: revision,
            element,
        };
        let encoded_bytes = serde_json::to_vec(&result)
            .map_err(|_| AccessibilityPlaneError::Internal)?
            .len();
        self.require_emitted_snapshot_bounds(1, encoded_bytes)?;
        Ok(result)
    }

    pub(crate) async fn snapshot_for(
        &self,
        request: ElementSnapshotRequest,
    ) -> Result<ElementSnapshotResult, AccessibilityPlaneError> {
        request
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        self.require_desktop(request.desktop_id, request.desktop_generation)?;
        require_supported_expansion(request.expansion)?;
        let expansion = request.expansion;
        self.hydrate_exact_expansion(
            &request.element,
            expansion,
            Instant::now() + Duration::from_millis(u64::from(self.config.query_timeout_ms)),
        )
        .await?;
        let permit = self.acquire_read_permit()?;
        let view = {
            let state = self.state.lock().await;
            require_ready(&state)?;
            // The mutable cache distinguishes absent from retained-stale births.
            state
                .cache
                .resolve_exact(&request.element)
                .map_err(map_model_error)?;
            require_exact_expansion_evidence(&state, &request.element, expansion)?;
            state.cache.snapshot()
        };
        let revision = view.revision();
        let element = self
            .run_admitted_read(permit, move || {
                view.resolve_exact(&request.element, request.expansion)
            })
            .await?
            .map_err(map_query_error)?;
        require_projected_expansion_evidence(std::slice::from_ref(&element), expansion)?;
        let state = self.state.lock().await;
        require_revision_current(&state, revision)?;
        let result = ElementSnapshotResult {
            snapshot_revision: revision,
            element,
        };
        let encoded_bytes = serde_json::to_vec(&result)
            .map_err(|_| AccessibilityPlaneError::Internal)?
            .len();
        self.require_emitted_snapshot_bounds(1, encoded_bytes)?;
        Ok(result)
    }

    /// Resolves one public birth into an opaque, exact actor-target proof.
    #[allow(
        dead_code,
        reason = "consumed by the semantic-action coordinator in the next integration slice"
    )]
    pub(crate) async fn resolve_action_target(
        &self,
        element: &ElementRef,
    ) -> Result<AccessibilityActionTargetEvidence, AccessibilityPlaneError> {
        let state = self.state.lock().await;
        resolve_action_target(&state, element)
    }

    /// Rejects any identity or global revision drift since target resolution.
    #[allow(
        dead_code,
        reason = "consumed by the semantic-action coordinator in the next integration slice"
    )]
    pub(crate) async fn revalidate_action_target(
        &self,
        evidence: &AccessibilityActionTargetEvidence,
    ) -> Result<AccessibilityActionTargetEvidence, AccessibilityPlaneError> {
        let state = self.state.lock().await;
        let current = resolve_action_target(&state, &evidence.current_element)?;
        if &current != evidence {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }
        Ok(current)
    }

    /// Queue-head proof that the daemon mirror still retains the exact child
    /// and selected top-level births admitted before queueing. Unrelated cache
    /// entries may advance the monotonic global revisions; the target-node
    /// revisions and identities may not. It never waits for the async plane
    /// lock: contention fails closed so an input-owner thread cannot block
    /// indefinitely.
    #[cfg(test)]
    pub(crate) fn revalidate_explicit_correlation_evidence_blocking(
        &self,
        evidence: &AccessibilityExplicitCorrelationEvidence,
    ) -> Result<(), AccessibilityPlaneError> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)?;
        let current_fence = accessibility_correlation_fence(&state)?;
        let current_element =
            resolve_action_target(&state, evidence.element_evidence.current_element())?;
        let current_target = resolve_action_target(
            &state,
            evidence.correlation_target.evidence().current_element(),
        )?;
        if current_fence.accessibility_generation != evidence.fence.accessibility_generation
            || current_fence.source_revision < evidence.fence.source_revision
            || current_fence.cache_revision < evidence.fence.cache_revision
            || !same_action_target_node(&current_element, &evidence.element_evidence)
            || !same_action_target_node(&current_target, evidence.correlation_target.evidence())
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }
        Ok(())
    }

    async fn reconcile_wait_reference(
        &self,
        element: &ElementRef,
        deadline: Instant,
    ) -> Result<AccessibilityPollAttempt, AccessibilityPlaneError> {
        let reconciler = self
            .poll_reconciler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(AccessibilityPlaneError::CapabilityUnavailable)?;
        let _poll_permit = Arc::clone(&self.poll_slots)
            .try_acquire_owned()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)?;
        let evidence = self.resolve_action_target(element).await?;
        let evidence = self.revalidate_action_target(&evidence).await?;
        if Instant::now() >= deadline {
            return Ok(AccessibilityPollAttempt::DeadlineElapsed { dispatched: false });
        }
        let expected_generation = evidence.accessibility_generation;
        let expected_source_revision = evidence.source_revision;
        let expected_node_revision = evidence.node_revision;
        let dispatch =
            match timeout_at(deadline, reconciler.reconcile_exact(evidence, deadline)).await {
                Ok(result) => result?,
                Err(_) => {
                    return Ok(AccessibilityPollAttempt::DeadlineElapsed { dispatched: true });
                }
            };
        if dispatch.accessibility_generation != expected_generation
            || dispatch.source_revision != expected_source_revision
            || dispatch.node_revision != expected_node_revision
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }
        Ok(AccessibilityPollAttempt::Reconciled)
    }

    /// Performs one bounded exact-reference metadata refresh, then waits for
    /// the actor's ordered mirror mutation before permitting a projection.
    /// Missing evidence remains capability-unavailable when no reconciler is
    /// installed, the toolkit rejects the read, or the deadline expires.
    async fn hydrate_exact_expansion(
        &self,
        element: &ElementRef,
        expansion: ElementSnapshotExpansion,
        deadline: Instant,
    ) -> Result<bool, AccessibilityPlaneError> {
        let reconciler = self
            .poll_reconciler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut changes = self.changes.subscribe();
        let evidence = {
            let state = self.state.lock().await;
            require_ready(&state)?;
            // Preserve the cache's public identity classification before
            // consulting mirror-only hydration evidence: unknown addresses
            // are not found, while removed or replaced births remain stale.
            state
                .cache
                .resolve_exact(element)
                .map_err(map_model_error)?;
            match require_exact_expansion_evidence(&state, element, expansion) {
                Ok(()) => return Ok(false),
                Err(AccessibilityPlaneError::CapabilityUnavailable) => {}
                Err(error) => return Err(error),
            }
            resolve_action_target(&state, element)?
        };
        let reconciler = reconciler.ok_or(AccessibilityPlaneError::CapabilityUnavailable)?;
        let _poll_permit = Arc::clone(&self.poll_slots)
            .try_acquire_owned()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)?;
        if Instant::now() >= deadline {
            return Err(AccessibilityPlaneError::CapabilityUnavailable);
        }
        let expected_generation = evidence.accessibility_generation;
        let expected_source_revision = evidence.source_revision;
        let expected_node_revision = evidence.node_revision;
        let dispatch = timeout_at(deadline, reconciler.reconcile_exact(evidence, deadline))
            .await
            .map_err(|_| AccessibilityPlaneError::CapabilityUnavailable)??;
        if dispatch.accessibility_generation != expected_generation
            || dispatch.source_revision != expected_source_revision
            || dispatch.node_revision != expected_node_revision
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }
        loop {
            {
                let state = self.state.lock().await;
                require_ready(&state)?;
                match require_exact_expansion_evidence(&state, element, expansion) {
                    Ok(()) => return Ok(true),
                    Err(AccessibilityPlaneError::CapabilityUnavailable) => {}
                    Err(error) => return Err(error),
                }
                changes.borrow_and_update();
            }
            if timeout_at(deadline, changes.changed()).await.is_err() {
                return Err(AccessibilityPlaneError::CapabilityUnavailable);
            }
        }
    }

    pub(crate) async fn wait_for(
        &self,
        mut request: ElementWaitRequest,
    ) -> Result<ElementWaitResult, AccessibilityPlaneError> {
        request
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        self.require_desktop(request.desktop_id, request.desktop_generation)?;
        require_supported_expansion(request.expansion)?;
        request.limits = self.effective_query_limits(request.limits)?;
        if request.allow_poll_fallback
            && matches!(
                &request.target,
                xenoteer_protocol::ElementWaitTarget::Selector { .. }
            )
        {
            return Err(AccessibilityPlaneError::CapabilityUnavailable);
        }
        let _permit = Arc::clone(&self.wait_slots)
            .try_acquire_owned()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)?;
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.timeout_ms));
        // Subscription precedes the first check. No ingest can occur between
        // the check and subscription, and watch retains the latest revision.
        let mut changes = self.changes.subscribe();
        let missing_exact_evidence = {
            let state = self.state.lock().await;
            require_ready(&state)?;
            match require_wait_evidence(&state, &request) {
                Ok(()) => None,
                Err(AccessibilityPlaneError::CapabilityUnavailable)
                    if request.allow_poll_fallback =>
                {
                    let ElementWaitTarget::Reference { element } = &request.target else {
                        return Err(AccessibilityPlaneError::CapabilityUnavailable);
                    };
                    Some(element.clone())
                }
                Err(error) => return Err(error),
            }
        };
        let mut poll_fallback_used = false;
        if let Some(element) = missing_exact_evidence {
            let hydration_expansion = wait_hydration_expansion(&request);
            poll_fallback_used = self
                .hydrate_exact_expansion(&element, hydration_expansion, deadline)
                .await?;
        }
        let initial_generation = {
            let state = self.state.lock().await;
            require_ready(&state)?;
            require_wait_evidence(&state, &request)?;
            state.cache.atspi_generation()
        };
        let mut deadline_elapsed = false;
        let mut poll_attempts = 0_usize;
        let poll_policy = *self
            .poll_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut poll_backoff = poll_policy.initial_backoff;
        loop {
            let query_deadline =
                AccessibilityQueryDeadline::from_timeout_ms(request.limits.timeout_ms)
                    .map_err(map_query_error)?;
            let wait_deadline = AccessibilityQueryDeadline::at(deadline.into_std());
            let evaluation_deadline = query_deadline.earliest(wait_deadline);
            // Equality belongs to the outer wait contract. A cooperative
            // timeout at that deadline is a normal timed-out wait result;
            // only a strictly earlier query budget is an endpoint error.
            let outer_wait_deadline_wins = evaluation_deadline == wait_deadline;
            let permit = self.acquire_read_permit()?;
            let view = {
                let state = self.state.lock().await;
                if state.resync_pending || state.cache.atspi_generation() != initial_generation {
                    return self.bounded_wait_result(resync_wait_result(
                        self,
                        &state,
                        poll_fallback_used,
                    ));
                }
                require_wait_evidence(&state, &request)?;
                state.cache.snapshot()
            };
            let evaluated_revision = view.revision();
            let request_for_evaluation = request.clone();
            let evaluation = match self
                .run_admitted_read(permit, move || {
                    view.evaluate_wait_with_deadline(&request_for_evaluation, evaluation_deadline)
                })
                .await?
            {
                Ok(evaluation) => evaluation,
                Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Timeout))
                    if outer_wait_deadline_wins =>
                {
                    let state = self.state.lock().await;
                    if state.resync_pending || state.cache.atspi_generation() != initial_generation
                    {
                        return self.bounded_wait_result(resync_wait_result(
                            self,
                            &state,
                            poll_fallback_used,
                        ));
                    }
                    return self.bounded_wait_result(wait_result(
                        self,
                        initial_generation,
                        ElementWaitStatus::TimedOut,
                        AccessibilityWaitEvaluation {
                            evaluated_revision,
                            predicate_satisfied: false,
                            selected_count: 0,
                            satisfying_count: 0,
                            satisfying_elements: Vec::new(),
                            warnings: Vec::new(),
                        },
                        poll_fallback_used,
                    ));
                }
                Err(error) => return Err(map_query_error(error)),
            };
            require_projected_expansion_evidence(
                &evaluation.satisfying_elements,
                request.expansion,
            )?;
            let state = self.state.lock().await;
            // Mark all changes preceding this serialized recheck as observed
            // while ingestion is excluded by the same state lock.
            changes.borrow_and_update();
            if state.resync_pending || state.cache.atspi_generation() != initial_generation {
                return self.bounded_wait_result(resync_wait_result(
                    self,
                    &state,
                    poll_fallback_used,
                ));
            }
            if state.cache.revision() != evaluation.evaluated_revision {
                if deadline_elapsed || Instant::now() >= deadline {
                    return self.bounded_wait_result(wait_result(
                        self,
                        initial_generation,
                        ElementWaitStatus::TimedOut,
                        evaluation,
                        poll_fallback_used,
                    ));
                }
                continue;
            }
            if evaluation.predicate_satisfied {
                return self.bounded_wait_result(wait_result(
                    self,
                    initial_generation,
                    ElementWaitStatus::Matched,
                    evaluation,
                    poll_fallback_used,
                ));
            }
            if deadline_elapsed || Instant::now() >= deadline {
                return self.bounded_wait_result(wait_result(
                    self,
                    initial_generation,
                    ElementWaitStatus::TimedOut,
                    evaluation,
                    poll_fallback_used,
                ));
            }
            drop(state);
            if !request.allow_poll_fallback || poll_attempts >= poll_policy.max_attempts {
                deadline_elapsed = timeout_at(deadline, changes.changed()).await.is_err();
                continue;
            }
            let poll_at = Instant::now()
                .checked_add(poll_backoff)
                .unwrap_or(deadline)
                .min(deadline);
            match timeout_at(poll_at, changes.changed()).await {
                Ok(_) => {}
                Err(_) if poll_at >= deadline => deadline_elapsed = true,
                Err(_) => {
                    let xenoteer_protocol::ElementWaitTarget::Reference { element } =
                        &request.target
                    else {
                        return Err(AccessibilityPlaneError::CapabilityUnavailable);
                    };
                    poll_attempts = poll_attempts.saturating_add(1);
                    match self.reconcile_wait_reference(element, deadline).await? {
                        AccessibilityPollAttempt::Reconciled => poll_fallback_used = true,
                        AccessibilityPollAttempt::DeadlineElapsed { dispatched } => {
                            poll_fallback_used |= dispatched;
                            deadline_elapsed = true;
                        }
                    }
                    poll_backoff = poll_backoff
                        .saturating_mul(2)
                        .min(poll_policy.maximum_backoff);
                }
            }
        }
    }

    fn effective_query_limits(
        &self,
        requested: AccessibilityQueryLimits,
    ) -> Result<AccessibilityQueryLimits, AccessibilityPlaneError> {
        let effective = AccessibilityQueryLimits {
            max_visited_nodes: requested
                .max_visited_nodes
                .min(self.config.max_nodes_per_query),
            max_depth: requested.max_depth.min(self.config.max_selector_depth),
            max_matches: requested.max_matches.min(self.config.max_query_matches),
            timeout_ms: requested.timeout_ms.min(self.config.query_timeout_ms),
        };
        effective
            .validate()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        Ok(effective)
    }

    fn effective_page_limit(&self, requested: u16, limits: AccessibilityQueryLimits) -> u16 {
        requested
            .min(limits.max_matches)
            .min(u16::try_from(self.config.max_snapshot_nodes).unwrap_or(u16::MAX))
    }

    fn require_emitted_snapshot_bounds(
        &self,
        node_count: usize,
        encoded_bytes: usize,
    ) -> Result<(), AccessibilityPlaneError> {
        let node_count =
            u32::try_from(node_count).map_err(|_| AccessibilityPlaneError::QueryLimitExceeded)?;
        if node_count > self.config.max_snapshot_nodes {
            return Err(AccessibilityPlaneError::QueryLimitExceeded);
        }
        let encoded_bytes = u32::try_from(encoded_bytes)
            .map_err(|_| AccessibilityPlaneError::QueryLimitExceeded)?;
        if encoded_bytes > self.config.max_snapshot_bytes {
            return Err(AccessibilityPlaneError::QueryLimitExceeded);
        }
        Ok(())
    }

    fn bounded_wait_result(
        &self,
        result: ElementWaitResult,
    ) -> Result<ElementWaitResult, AccessibilityPlaneError> {
        let encoded_bytes = serde_json::to_vec(&result)
            .map_err(|_| AccessibilityPlaneError::Internal)?
            .len();
        self.require_emitted_snapshot_bounds(result.elements.len(), encoded_bytes)?;
        Ok(result)
    }

    fn require_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), AccessibilityPlaneError> {
        if desktop_id != self.desktop_id {
            return Err(AccessibilityPlaneError::NotFound);
        }
        if desktop_generation != self.desktop_generation {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        Ok(())
    }

    fn acquire_read_permit(&self) -> Result<OwnedSemaphorePermit, AccessibilityPlaneError> {
        Arc::clone(&self.read_slots)
            .try_acquire_owned()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)
    }

    /// Installs or removes the runtime exact-reference poll reconciler.
    #[allow(
        dead_code,
        reason = "runtime binding follows the independently tested injected plane seam"
    )]
    pub(crate) fn set_poll_reconciler(
        &self,
        reconciler: Option<Arc<dyn AccessibilityPollReconciler>>,
    ) {
        *self
            .poll_reconciler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reconciler;
    }

    #[cfg(test)]
    fn set_poll_policy(
        &self,
        policy: AccessibilityPollPolicy,
    ) -> Result<(), AccessibilityPlaneError> {
        if policy.initial_backoff.is_zero()
            || policy.initial_backoff > policy.maximum_backoff
            || policy.max_attempts == 0
        {
            return Err(AccessibilityPlaneError::InvalidRequest);
        }
        *self
            .poll_policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
        Ok(())
    }

    async fn run_admitted_read<T, F>(
        &self,
        permit: OwnedSemaphorePermit,
        operation: F,
    ) -> Result<T, AccessibilityPlaneError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        #[cfg(test)]
        let hook = self
            .read_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            operation()
        })
        .await
        .map_err(|_| AccessibilityPlaneError::Internal)
    }

    #[cfg(test)]
    async fn run_bounded_read<T, F>(&self, operation: F) -> Result<T, AccessibilityPlaneError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self.acquire_read_permit()?;
        self.run_admitted_read(permit, operation).await
    }

    #[cfg(test)]
    fn set_read_test_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .read_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }

    /// Admits one page from an authoritative, common-revision cache copy.
    /// Nothing becomes queryable until the final page has been validated and
    /// installed atomically.
    pub(crate) async fn ingest_cache_page(
        &self,
        page: CachePage,
    ) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
        let validation = validate_cache_page(&page, self.config);
        let mut state = self.state.lock().await;
        let page_bytes = match validation {
            Ok(page_bytes) => page_bytes,
            Err(error) => {
                if state.bootstrap.is_some() {
                    force_resync(&mut state);
                    let _ = changed(
                        &mut state,
                        &self.changes,
                        AccessibilityIngestKind::ResyncRequired,
                    );
                    return Err(AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    });
                }
                return Err(error);
            }
        };
        let starts_bootstrap = state.bootstrap.is_none();
        if starts_bootstrap {
            let current = state.cache.atspi_generation().get();
            let generation_allowed = state.resync_pending
                && state.expected_actor_generation == Some(page.accessibility_generation)
                && page.accessibility_generation >= current;
            if !generation_allowed {
                return Err(AccessibilityPlaneError::StaleReference {
                    current_generation: Some(self.desktop_generation),
                });
            }
        }
        if starts_bootstrap {
            state.bootstrap = Some(BootstrapAccumulator {
                accessibility_generation: page.accessibility_generation,
                source_revision: page.revision,
                nodes: BTreeMap::new(),
                estimated_bytes: 0,
                expected_after: None,
            });
            state.resync_pending = true;
        }
        let accumulator = state
            .bootstrap
            .as_mut()
            .ok_or(AccessibilityPlaneError::Internal)?;
        if accumulator.accessibility_generation != page.accessibility_generation
            || accumulator.source_revision != page.revision
            || accumulator.expected_after != page.after
            || (accumulator.expected_after.is_some() && page.nodes.is_empty())
        {
            force_resync(&mut state);
            return changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            )
            .and(Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            }));
        }
        let Some(estimated_bytes) = accumulator.estimated_bytes.checked_add(page_bytes) else {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResourceExhausted);
        };
        accumulator.estimated_bytes = estimated_bytes;
        if accumulator.estimated_bytes > bootstrap_byte_limit(self.config) {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResourceExhausted);
        }
        for node in page.nodes {
            if accumulator
                .nodes
                .insert(node.item.object.clone(), node)
                .is_some()
                || accumulator.nodes.len() > self.config.model_limits.max_live_nodes
                || accumulator.nodes.len() > self.config.raw_cache_limits.max_nodes
            {
                force_resync(&mut state);
                let _ = changed(
                    &mut state,
                    &self.changes,
                    AccessibilityIngestKind::ResyncRequired,
                );
                return Err(AccessibilityPlaneError::ResourceExhausted);
            }
        }
        accumulator.expected_after = page.next_after.clone();
        if page.next_after.is_some() {
            return changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::BootstrapPending,
            );
        }
        let complete = state
            .bootstrap
            .take()
            .ok_or(AccessibilityPlaneError::Internal)?;
        state.has_bootstrapped = true;
        let authoritative_generation = AtspiGeneration::new(complete.accessibility_generation)
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
        state.cache = AccessibilityCache::new(
            self.desktop_id,
            self.desktop_generation,
            authoritative_generation,
            self.config.model_limits,
        )
        .map_err(map_model_error)?;
        state.raw_nodes = complete.nodes;
        state.raw_estimated_bytes = complete.estimated_bytes;
        state.applications.clear();
        state.actor_application_generations.clear();
        state.elements.clear();
        if validate_authoritative_topology(&state.raw_nodes).is_err() {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        if let Err(error) = rebuild_all(&mut state) {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(error);
        }
        if state.cache.snapshot().graph_status().resync_required {
            force_resync(&mut state);
            return changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            )
            .and(Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            }));
        }
        state.source_revision = Some(complete.source_revision);
        state.resync_pending = false;
        state.expected_actor_generation = None;
        changed(&mut state, &self.changes, AccessibilityIngestKind::Rebuilt)
    }

    /// Applies one ordered incremental mirror mutation.
    pub(crate) async fn ingest_mutation(
        &self,
        accessibility_generation: u64,
        previous_revision: u64,
        mutation: CacheMutation,
    ) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
        let mut state = self.state.lock().await;
        if state.resync_pending || state.bootstrap.is_some() {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        if accessibility_generation != state.cache.atspi_generation().get() {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        if mutation.kind == CacheMutationKind::Unchanged
            && mutation.detail == CacheMutationDetail::Unchanged
            && state.source_revision == Some(previous_revision)
            && mutation.revision == previous_revision
        {
            return Ok(AccessibilityIngestEvent {
                desktop_id: state.desktop_id,
                desktop_generation: state.desktop_generation,
                kind: AccessibilityIngestKind::Unchanged,
                atspi_generation: state.cache.atspi_generation(),
                revision: state.cache.revision(),
                cache_sequence: event_cache_sequence(&state),
                sources: Vec::new(),
            });
        }
        let expected_revision = state
            .source_revision
            .and_then(|revision| revision.checked_add(1));
        if state.source_revision.is_none()
            || state.source_revision != Some(previous_revision)
            || mutation.revision == 0
            || expected_revision != Some(mutation.revision)
        {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        let applied = match (mutation.kind, mutation.detail) {
            (CacheMutationKind::Upserted, CacheMutationDetail::Upserted(node)) => {
                let node = *node;
                let raw_admission =
                    validate_cached_node(&node, mutation.revision, self.config.raw_cache_limits)
                        .and_then(|node_bytes| {
                            prospective_raw_upsert_bytes(
                                &state,
                                &node,
                                node_bytes,
                                self.config.raw_cache_limits.max_total_bytes,
                            )
                            .map(|total_bytes| (node_bytes, total_bytes))
                        });
                if raw_admission.is_err()
                    || node.revision != mutation.revision
                    || node.item.parent.as_ref().is_some_and(|parent| {
                        parent == &node.item.object || !state.raw_nodes.contains_key(parent)
                    })
                {
                    force_resync(&mut state);
                    let _ = changed(
                        &mut state,
                        &self.changes,
                        AccessibilityIngestKind::ResyncRequired,
                    );
                    return Err(AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    });
                }
                let (_, total_bytes) =
                    raw_admission.map_err(|_| AccessibilityPlaneError::Internal)?;
                let address = node.item.object.clone();
                apply_upsert(&mut state, node, total_bytes).and_then(|()| {
                    Ok((
                        AccessibilityIngestKind::Upserted,
                        vec![resolved_ingest_source(&state, &address)?],
                    ))
                })
            }
            (CacheMutationKind::Refreshed, CacheMutationDetail::Refreshed(node)) => {
                let node = *node;
                let raw_admission =
                    validate_cached_node(&node, mutation.revision, self.config.raw_cache_limits)
                        .and_then(|node_bytes| {
                            prospective_raw_upsert_bytes(
                                &state,
                                &node,
                                node_bytes,
                                self.config.raw_cache_limits.max_total_bytes,
                            )
                            .map(|total_bytes| (node_bytes, total_bytes))
                        });
                if raw_admission.is_err() || node.revision != mutation.revision {
                    force_resync(&mut state);
                    let _ = changed(
                        &mut state,
                        &self.changes,
                        AccessibilityIngestKind::ResyncRequired,
                    );
                    return Err(AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    });
                }
                let (_, total_bytes) =
                    raw_admission.map_err(|_| AccessibilityPlaneError::Internal)?;
                let address = node.item.object.clone();
                apply_refresh(&mut state, node, total_bytes).and_then(|()| {
                    Ok((
                        AccessibilityIngestKind::Refreshed,
                        vec![resolved_ingest_source(&state, &address)?],
                    ))
                })
            }
            (CacheMutationKind::Removed, CacheMutationDetail::Removed(addresses)) => {
                let unique = addresses.iter().cloned().collect::<BTreeSet<_>>();
                if addresses.is_empty()
                    || unique.len() != addresses.len()
                    || unique.iter().any(|address| {
                        !state.raw_nodes.contains_key(address)
                            || !state.elements.contains_key(address)
                    })
                {
                    force_resync(&mut state);
                    let _ = changed(
                        &mut state,
                        &self.changes,
                        AccessibilityIngestKind::ResyncRequired,
                    );
                    return Err(AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    });
                }
                let sources = addresses
                    .iter()
                    .map(|address| resolved_ingest_source(&state, address))
                    .collect::<Result<Vec<_>, _>>()?;
                apply_removed(&mut state, addresses)
                    .map(|()| (AccessibilityIngestKind::Removed, sources))
            }
            (
                CacheMutationKind::ApplicationInvalidated,
                CacheMutationDetail::ApplicationInvalidated {
                    bus_name,
                    application_generation,
                    removed,
                },
            ) => {
                let current = state
                    .raw_nodes
                    .keys()
                    .filter(|address| address.bus_name() == bus_name)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let announced = removed.iter().cloned().collect::<BTreeSet<_>>();
                let expected_application_generation = state
                    .actor_application_generations
                    .get(&bus_name)
                    .and_then(|generation| generation.checked_add(1));
                if current.is_empty()
                    || announced.len() != removed.len()
                    || announced != current
                    || expected_application_generation != Some(application_generation)
                {
                    force_resync(&mut state);
                    let _ = changed(
                        &mut state,
                        &self.changes,
                        AccessibilityIngestKind::ResyncRequired,
                    );
                    return Err(AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    });
                }
                let sources = removed
                    .iter()
                    .map(|address| resolved_ingest_source(&state, address))
                    .collect::<Result<Vec<_>, _>>()?;
                match apply_application_invalidation(&mut state, &bus_name) {
                    Ok(()) => {
                        state
                            .actor_application_generations
                            .insert(bus_name, application_generation);
                        Ok((AccessibilityIngestKind::ApplicationInvalidated, sources))
                    }
                    Err(error) => Err(error),
                }
            }
            (CacheMutationKind::ResyncRequired, CacheMutationDetail::ResyncRequired) => {
                force_resync(&mut state);
                return changed(
                    &mut state,
                    &self.changes,
                    AccessibilityIngestKind::ResyncRequired,
                );
            }
            _ => {
                force_resync(&mut state);
                let _ = changed(
                    &mut state,
                    &self.changes,
                    AccessibilityIngestKind::ResyncRequired,
                );
                return Err(AccessibilityPlaneError::ResyncRequired {
                    current_generation: Some(self.desktop_generation),
                });
            }
        };
        let (kind, sources) = match applied {
            Ok(applied) => applied,
            Err(error) => {
                force_resync(&mut state);
                let _ = changed(
                    &mut state,
                    &self.changes,
                    AccessibilityIngestKind::ResyncRequired,
                );
                return Err(if error == AccessibilityPlaneError::Internal {
                    AccessibilityPlaneError::Internal
                } else {
                    AccessibilityPlaneError::ResyncRequired {
                        current_generation: Some(self.desktop_generation),
                    }
                });
            }
        };
        if state.cache.snapshot().graph_status().resync_required {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        state.source_revision = Some(mutation.revision);
        changed_with_sources(&mut state, &self.changes, kind, sources)
    }

    /// Begins an actor-authorized resynchronization and binds the next full
    /// bootstrap to exactly the announced generation.
    pub(crate) async fn begin_resync(
        &self,
        actor_generation: u64,
        _reason: AccessibilityResyncCause,
    ) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
        let mut state = self.state.lock().await;
        let current = state.cache.atspi_generation().get();
        if actor_generation <= current
            || state
                .expected_actor_generation
                .is_some_and(|expected| actor_generation < expected)
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        force_resync(&mut state);
        state.expected_actor_generation = Some(actor_generation);
        changed(
            &mut state,
            &self.changes,
            AccessibilityIngestKind::ResyncRequired,
        )
    }

    /// Applies the actor's separate application-owner invalidation event.
    pub(crate) async fn ingest_application_invalidation(
        &self,
        accessibility_generation: u64,
        cache_revision: u64,
        bus_name: String,
        application_generation: u64,
    ) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
        if application_generation == 0 || AtspiBusName::new(bus_name.clone()).is_err() {
            return Err(AccessibilityPlaneError::InvalidRequest);
        }
        let mut state = self.state.lock().await;
        require_ready(&state)?;
        if accessibility_generation != state.cache.atspi_generation().get()
            || state
                .source_revision
                .is_some_and(|revision| cache_revision < revision)
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        if state.source_revision == Some(cache_revision)
            && state.actor_application_generations.get(&bus_name) == Some(&application_generation)
            && !state
                .raw_nodes
                .keys()
                .any(|address| address.bus_name() == bus_name)
        {
            return Ok(AccessibilityIngestEvent {
                desktop_id: state.desktop_id,
                desktop_generation: state.desktop_generation,
                kind: AccessibilityIngestKind::Unchanged,
                atspi_generation: state.cache.atspi_generation(),
                revision: state.cache.revision(),
                cache_sequence: event_cache_sequence(&state),
                sources: Vec::new(),
            });
        }
        force_resync(&mut state);
        let _ = changed(
            &mut state,
            &self.changes,
            AccessibilityIngestKind::ResyncRequired,
        );
        Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: Some(self.desktop_generation),
        })
    }

    /// Resolves one actor event source against an exact mirrored cache coordinate.
    ///
    /// A delayed or already-removed object remains publishable through its raw
    /// address, but never inherits a newer object's public birth.
    pub(crate) async fn resolve_event_source(
        &self,
        accessibility_generation: u64,
        source_revision: u64,
        raw: ObjectAddress,
    ) -> Result<AccessibilitySourceResolution, AccessibilityPlaneError> {
        let state = self.state.lock().await;
        require_ready(&state)?;
        if accessibility_generation != state.cache.atspi_generation().get() {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: Some(self.desktop_generation),
            });
        }
        let mirrored_source_revision =
            state
                .source_revision
                .ok_or(AccessibilityPlaneError::ResyncRequired {
                    current_generation: Some(self.desktop_generation),
                })?;
        if source_revision > mirrored_source_revision {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        let source =
            if source_revision == mirrored_source_revision && state.elements.contains_key(&raw) {
                resolved_ingest_source(&state, &raw)?
            } else {
                AccessibilityIngestSource {
                    raw,
                    element: None,
                    metadata: AccessibilityEventMetadata::default(),
                }
            };
        Ok(AccessibilitySourceResolution {
            desktop_id: state.desktop_id,
            desktop_generation: state.desktop_generation,
            atspi_generation: state.cache.atspi_generation(),
            revision: state.cache.revision(),
            cache_sequence: source.element.as_ref().map_or_else(
                || event_cache_sequence(&state),
                |element| element.cache_sequence,
            ),
            source,
        })
    }

    /// Enumerates the complete bounded application/top-level correlation set.
    #[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
    pub(crate) async fn accessibility_correlation_targets(
        &self,
    ) -> Result<AccessibilityCorrelationTargets, AccessibilityPlaneError> {
        let state = self.state.lock().await;
        Ok(AccessibilityCorrelationTargets {
            fence: accessibility_correlation_fence(&state)?,
            targets: accessibility_correlation_targets(&state)?,
        })
    }

    /// Captures an exact child and the complete app/top-level target set under
    /// one fence. The caller must use the child's fresh `top_level` address to
    /// select the target; a child is never searched for in the target set.
    pub(crate) async fn accessibility_element_correlation_context(
        &self,
        element: &ElementRef,
    ) -> Result<AccessibilityElementCorrelationContext, AccessibilityPlaneError> {
        let state = self.state.lock().await;
        Ok(AccessibilityElementCorrelationContext {
            fence: accessibility_correlation_fence(&state)?,
            element_evidence: resolve_action_target(&state, element)?,
            targets: accessibility_explicit_correlation_targets(&state)?,
        })
    }

    /// Captures the current child proof and correlation target universe for a
    /// synchronous input queue-head check. Lock contention is a bounded,
    /// fail-closed resource error; the input owner never waits on async work.
    pub(crate) fn accessibility_element_correlation_context_blocking(
        &self,
        element: &ElementRef,
    ) -> Result<AccessibilityElementCorrelationContext, AccessibilityPlaneError> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| AccessibilityPlaneError::ResourceExhausted)?;
        Ok(AccessibilityElementCorrelationContext {
            fence: accessibility_correlation_fence(&state)?,
            element_evidence: resolve_action_target(&state, element)?,
            targets: accessibility_explicit_correlation_targets(&state)?,
        })
    }

    /// Atomically replaces all evidence-bearing correlations at one exact
    /// enumeration fence. Empty assignments clear every inherited correlation.
    #[allow(dead_code, reason = "consumed by the deferred correlation coordinator")]
    pub(crate) async fn replace_window_correlations(
        &self,
        fence: AccessibilityCorrelationFence,
        assignments: Vec<AccessibilityCorrelationAssignment>,
    ) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
        if assignments.len() > MAX_ACCESSIBILITY_CORRELATION_CANDIDATES {
            return Err(AccessibilityPlaneError::ResourceExhausted);
        }
        let mut state = self.state.lock().await;
        require_ready(&state)?;
        if fence.accessibility_generation != state.cache.atspi_generation().get()
            || Some(fence.source_revision) != state.source_revision
            || fence.cache_revision != state.cache.revision()
        {
            return Err(AccessibilityPlaneError::StaleReference {
                current_generation: None,
            });
        }

        let mut desired = BTreeMap::new();
        for assignment in assignments {
            if assignment.evidence.accessibility_generation != fence.accessibility_generation
                || assignment.evidence.source_revision != fence.source_revision
                || assignment.evidence.cache_revision != fence.cache_revision
                || assignment
                    .correlation
                    .window
                    .as_ref()
                    .is_some_and(|window| {
                        window.desktop_id != self.desktop_id
                            || window.desktop_generation != self.desktop_generation
                    })
            {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
            let current = resolve_action_target(&state, &assignment.evidence.current_element)?;
            if current != assignment.evidence {
                return Err(AccessibilityPlaneError::StaleReference {
                    current_generation: None,
                });
            }
            let mut validation_snapshot = state
                .cache
                .resolve_exact(&current.current_element)
                .map_err(map_model_error)?
                .clone();
            validation_snapshot.window_correlation = assignment.correlation.clone();
            validation_snapshot
                .validate()
                .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
            let target_node = state.raw_nodes.get(&current.object).ok_or(
                AccessibilityPlaneError::ResyncRequired {
                    current_generation: None,
                },
            )?;
            if !is_correlation_target(target_node)
                || desired
                    .insert(
                        current.object.clone(),
                        InstalledWindowCorrelation {
                            evidence: current,
                            correlation: assignment.correlation,
                        },
                    )
                    .is_some()
            {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
        }

        let public_unchanged = desired.len() == state.correlations.len()
            && desired.iter().all(|(address, desired)| {
                state.correlations.get(address).is_some_and(|current| {
                    current.evidence.current_element == desired.evidence.current_element
                        && current.correlation == desired.correlation
                })
            });
        state.correlations = desired;
        if public_unchanged {
            return Ok(AccessibilityIngestEvent {
                desktop_id: state.desktop_id,
                desktop_generation: state.desktop_generation,
                kind: AccessibilityIngestKind::Unchanged,
                atspi_generation: state.cache.atspi_generation(),
                revision: state.cache.revision(),
                cache_sequence: event_cache_sequence(&state),
                sources: Vec::new(),
            });
        }
        if refresh_all_normalized_snapshots(&mut state).is_err() {
            force_resync(&mut state);
            let _ = changed(
                &mut state,
                &self.changes,
                AccessibilityIngestKind::ResyncRequired,
            );
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: Some(self.desktop_generation),
            });
        }
        changed(
            &mut state,
            &self.changes,
            AccessibilityIngestKind::Correlated,
        )
    }
}

impl AccessibilityPlane for DaemonAccessibilityPlane {
    fn list_elements<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementListRequest,
    ) -> AccessibilityFuture<'a, Result<ElementListPage, AccessibilityPlaneError>> {
        Box::pin(async move { self.list_for(context.principal().id(), request).await })
    }

    fn query_elements<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementQueryRequest,
    ) -> AccessibilityFuture<'a, Result<ElementQueryPage, AccessibilityPlaneError>> {
        Box::pin(async move { self.query_for(context.principal().id(), request).await })
    }

    fn resolve_element<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementResolveRequest,
    ) -> AccessibilityFuture<'a, Result<ElementResolveResult, AccessibilityPlaneError>> {
        Box::pin(async move { self.resolve_for(request).await })
    }

    fn element_snapshot<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementSnapshotRequest,
    ) -> AccessibilityFuture<'a, Result<ElementSnapshotResult, AccessibilityPlaneError>> {
        Box::pin(async move { self.snapshot_for(request).await })
    }

    fn wait_element<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementWaitRequest,
    ) -> AccessibilityFuture<'a, Result<ElementWaitResult, AccessibilityPlaneError>> {
        Box::pin(async move { self.wait_for(request).await })
    }
}

fn require_ready(state: &PlaneState) -> Result<(), AccessibilityPlaneError> {
    if state.resync_pending || state.bootstrap.is_some() {
        Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn same_action_target_node(
    current: &AccessibilityActionTargetEvidence,
    admitted: &AccessibilityActionTargetEvidence,
) -> bool {
    current.object == admitted.object
        && current.application == admitted.application
        && current.accessibility_generation == admitted.accessibility_generation
        && current.application_generation == admitted.application_generation
        && current.node_revision == admitted.node_revision
        && current.current_element == admitted.current_element
}

fn require_supported_expansion(
    expansion: ElementSnapshotExpansion,
) -> Result<(), AccessibilityPlaneError> {
    if expansion.actions || expansion.text_content || expansion.attributes || expansion.relations {
        // These fields are reserved in the Rust model for a future bounded
        // hydration contract, but are deliberately absent from the wire
        // schema in this protocol version.
        return Err(AccessibilityPlaneError::InvalidRequest);
    }
    Ok(())
}

fn require_exact_expansion_evidence(
    state: &PlaneState,
    element: &ElementRef,
    expansion: ElementSnapshotExpansion,
) -> Result<(), AccessibilityPlaneError> {
    let object = ObjectAddress::new(
        element.application.unique_bus_name.as_str(),
        element.object_path.as_str(),
    )
    .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
    if state.elements.get(&object) != Some(element) {
        return Err(AccessibilityPlaneError::StaleReference {
            current_generation: None,
        });
    }
    let node = state
        .raw_nodes
        .get(&object)
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    require_node_expansion_evidence(node, expansion)
}

fn require_node_expansion_evidence(
    node: &CachedNode,
    expansion: ElementSnapshotExpansion,
) -> Result<(), AccessibilityPlaneError> {
    let protected = map_role(node.item.role) == ElementRole::PasswordText
        || !matches!(node.item.text_protection, TextProtection::Unprotected);
    if (expansion.value
        && !protected
        && node_has_interface(node, ElementInterface::Value)
        && node.live.value.is_none())
        || (expansion.text_metadata
            && !protected
            && node_has_interface(node, ElementInterface::Text)
            && node.live.text.is_none())
        || (expansion.component
            && node_has_interface(node, ElementInterface::Component)
            && node.live.bounds.is_none())
    {
        return Err(AccessibilityPlaneError::CapabilityUnavailable);
    }
    Ok(())
}

fn require_projected_expansion_evidence(
    entries: &[ElementSnapshotEntry],
    expansion: ElementSnapshotExpansion,
) -> Result<(), AccessibilityPlaneError> {
    for entry in entries {
        let snapshot = &entry.snapshot;
        if (expansion.value
            && !snapshot.is_protected()
            && snapshot.interfaces.contains(&ElementInterface::Value)
            && snapshot.value.is_none())
            || (expansion.text_metadata
                && !snapshot.is_protected()
                && snapshot.interfaces.contains(&ElementInterface::Text)
                && snapshot.text.is_none())
            || (expansion.component
                && snapshot.interfaces.contains(&ElementInterface::Component)
                && snapshot.component.is_none())
        {
            return Err(AccessibilityPlaneError::CapabilityUnavailable);
        }
    }
    Ok(())
}

fn require_selector_evidence(
    state: &PlaneState,
    selector: &ElementSelector,
) -> Result<(), AccessibilityPlaneError> {
    for predicate in &selector.predicates {
        match predicate {
            ElementPredicate::AccessibleId { .. }
            | ElementPredicate::Attribute { .. }
            | ElementPredicate::Action { .. }
            | ElementPredicate::Relation { .. } => {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
            ElementPredicate::ValueRange { .. } => require_complete_value_evidence(state)?,
            ElementPredicate::ComponentIntersects { .. } => {
                require_complete_component_evidence(state)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn require_wait_evidence(
    state: &PlaneState,
    request: &ElementWaitRequest,
) -> Result<(), AccessibilityPlaneError> {
    if let ElementWaitTarget::Selector { selector, .. } = &request.target {
        require_selector_evidence(state, selector)?;
    }
    match &request.predicate {
        ElementWaitPredicate::Text { .. } => Err(AccessibilityPlaneError::InvalidRequest),
        ElementWaitPredicate::Value { .. } => match &request.target {
            ElementWaitTarget::Reference { element } => {
                let expansion = ElementSnapshotExpansion {
                    actions: false,
                    value: true,
                    text_metadata: false,
                    text_content: false,
                    attributes: false,
                    relations: false,
                    component: false,
                };
                require_exact_expansion_evidence(state, element, expansion)
            }
            ElementWaitTarget::Selector { .. } => require_complete_value_evidence(state),
        },
        ElementWaitPredicate::Geometry { .. } => match &request.target {
            ElementWaitTarget::Reference { element } => {
                let expansion = ElementSnapshotExpansion {
                    component: true,
                    ..ElementSnapshotExpansion::default()
                };
                require_exact_expansion_evidence(state, element, expansion)
            }
            ElementWaitTarget::Selector { .. } => require_complete_component_evidence(state),
        },
        _ => Ok(()),
    }
}

fn wait_hydration_expansion(request: &ElementWaitRequest) -> ElementSnapshotExpansion {
    let mut expansion = request.expansion;
    match &request.predicate {
        ElementWaitPredicate::Value { .. } => expansion.value = true,
        ElementWaitPredicate::Geometry { .. } => expansion.component = true,
        _ => {}
    }
    expansion
}

fn require_complete_value_evidence(state: &PlaneState) -> Result<(), AccessibilityPlaneError> {
    for node in state.raw_nodes.values() {
        let protected = map_role(node.item.role) == ElementRole::PasswordText
            || !matches!(node.item.text_protection, TextProtection::Unprotected);
        if !protected
            && node_has_interface(node, ElementInterface::Value)
            && node.live.value.is_none()
        {
            return Err(AccessibilityPlaneError::CapabilityUnavailable);
        }
    }
    Ok(())
}

fn require_complete_component_evidence(state: &PlaneState) -> Result<(), AccessibilityPlaneError> {
    for node in state.raw_nodes.values() {
        if node_has_interface(node, ElementInterface::Component) && node.live.bounds.is_none() {
            return Err(AccessibilityPlaneError::CapabilityUnavailable);
        }
    }
    Ok(())
}

fn node_has_interface(node: &CachedNode, expected: ElementInterface) -> bool {
    node.item
        .interfaces
        .iter()
        .filter_map(|interface| map_interface(interface))
        .any(|interface| interface == expected)
}

fn resolve_action_target(
    state: &PlaneState,
    element: &ElementRef,
) -> Result<AccessibilityActionTargetEvidence, AccessibilityPlaneError> {
    require_ready(state)?;
    state
        .cache
        .resolve_exact(element)
        .map_err(map_model_error)?;
    let object = ObjectAddress::new(
        element.application.unique_bus_name.as_str(),
        element.object_path.as_str(),
    )
    .map_err(|_| AccessibilityPlaneError::InvalidRequest)?;
    let current_element =
        state
            .elements
            .get(&object)
            .ok_or(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            })?;
    let node = state
        .raw_nodes
        .get(&object)
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    let source_revision = state
        .source_revision
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    let actor_application_generation = state
        .actor_application_generations
        .get(node.item.application.bus_name())
        .copied()
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    if current_element != element
        || node.item.object != object
        || node.application_generation != actor_application_generation
        || node.revision > source_revision
    {
        return Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        });
    }
    Ok(AccessibilityActionTargetEvidence {
        object,
        application: node.item.application.clone(),
        accessibility_generation: state.cache.atspi_generation().get(),
        application_generation: actor_application_generation,
        source_revision,
        cache_revision: state.cache.revision(),
        node_revision: node.revision,
        current_element: current_element.clone(),
    })
}

fn accessibility_correlation_fence(
    state: &PlaneState,
) -> Result<AccessibilityCorrelationFence, AccessibilityPlaneError> {
    require_ready(state)?;
    Ok(AccessibilityCorrelationFence {
        accessibility_generation: state.cache.atspi_generation().get(),
        source_revision: state
            .source_revision
            .ok_or(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            })?,
        cache_revision: state.cache.revision(),
    })
}

fn accessibility_correlation_targets(
    state: &PlaneState,
) -> Result<Vec<AccessibilityCorrelationTarget>, AccessibilityPlaneError> {
    let addresses = state
        .raw_nodes
        .iter()
        .filter(|(_, node)| is_correlation_target(node))
        .map(|(address, _)| address.clone())
        .collect::<BTreeSet<_>>();
    accessibility_correlation_targets_for_addresses(state, addresses)
}

/// Explicit element-to-window correlation follows the same ancestry rule as
/// the actor's fresh top-level read. Some toolkits expose the direct child of
/// the application root with a generic role, so role-only enumeration would
/// discard an otherwise exact, bounded caller-anchored correlation target.
fn accessibility_explicit_correlation_targets(
    state: &PlaneState,
) -> Result<Vec<AccessibilityCorrelationTarget>, AccessibilityPlaneError> {
    let addresses = state
        .raw_nodes
        .iter()
        .filter(|(_, node)| {
            is_correlation_target(node) || node.item.parent.as_ref() == Some(&node.item.application)
        })
        .map(|(address, _)| address.clone())
        .collect::<BTreeSet<_>>();
    accessibility_correlation_targets_for_addresses(state, addresses)
}

fn accessibility_correlation_targets_for_addresses(
    state: &PlaneState,
    addresses: BTreeSet<ObjectAddress>,
) -> Result<Vec<AccessibilityCorrelationTarget>, AccessibilityPlaneError> {
    if addresses.len() > MAX_ACCESSIBILITY_CORRELATION_CANDIDATES {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    let mut targets = Vec::with_capacity(addresses.len());
    for address in addresses {
        let element =
            state
                .elements
                .get(&address)
                .ok_or(AccessibilityPlaneError::ResyncRequired {
                    current_generation: None,
                })?;
        let evidence = resolve_action_target(state, element)?;
        let snapshot = state
            .cache
            .resolve_exact(element)
            .map_err(map_model_error)?
            .clone();
        let application_name = state
            .raw_nodes
            .get(&evidence.application)
            .and_then(|application| nonempty(application.item.name.clone()));
        targets.push(AccessibilityCorrelationTarget {
            evidence,
            snapshot,
            application_name,
        });
    }
    Ok(targets)
}

fn require_revision_current(
    state: &PlaneState,
    revision: AccessibilityRevision,
) -> Result<(), AccessibilityPlaneError> {
    require_ready(state)?;
    if state.cache.revision() != revision {
        return Err(AccessibilityPlaneError::StaleReference {
            current_generation: None,
        });
    }
    Ok(())
}

fn require_projection_current(
    state: &PlaneState,
    projection: &AccessibilityQueryProjection,
) -> Result<(), AccessibilityPlaneError> {
    require_revision_current(state, projection.snapshot_revision)?;
    if state.cache.atspi_generation() != projection.atspi_generation {
        return Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        });
    }
    Ok(())
}

fn without_list_cursor(mut request: ElementListRequest) -> ElementListRequest {
    request.cursor = None;
    request
}

fn list_cursor_binding(
    request: &ElementListRequest,
) -> Result<CursorBinding, AccessibilityPlaneError> {
    let request = without_list_cursor(request.clone());
    let encoded = serde_json::to_vec(&request).map_err(|_| AccessibilityPlaneError::Internal)?;
    Ok(CursorBinding {
        digest: request_binding_digest(b"list-v1", &encoded),
        kind: CursorRequestKind::List,
        order: request.order,
        limit: request.limit,
        expansion: request.expansion,
        limits: request.limits,
    })
}

fn query_cursor_binding(
    request: &ElementQueryRequest,
) -> Result<CursorBinding, AccessibilityPlaneError> {
    let request = without_query_cursor(request.clone());
    let encoded = serde_json::to_vec(&request).map_err(|_| AccessibilityPlaneError::Internal)?;
    Ok(CursorBinding {
        digest: request_binding_digest(b"query-v1", &encoded),
        kind: CursorRequestKind::Query,
        order: request.selector.order,
        limit: request.limit,
        expansion: request.expansion,
        limits: request.limits,
    })
}

fn request_binding_digest(domain: &[u8], encoded: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"xenoteer-daemon-accessibility-cursor\0");
    digest.update((domain.len() as u64).to_be_bytes());
    digest.update(domain);
    digest.update((encoded.len() as u64).to_be_bytes());
    digest.update(encoded);
    digest.finalize().into()
}

fn without_query_cursor(mut request: ElementQueryRequest) -> ElementQueryRequest {
    request.cursor = None;
    request
}

fn consume_cursor(
    state: &mut PlaneState,
    principal: &str,
    cursor: &AccessibilityPageCursor,
    binding: &CursorBinding,
) -> Result<AccessibilityContinuationDescriptor, AccessibilityPlaneError> {
    let now = Instant::now();
    purge_expired_cursors(state, now);
    let token_digest = cursor_token_digest(cursor.as_str());
    let Some(record) = state.cursors.get(&token_digest) else {
        return Err(AccessibilityPlaneError::StaleReference {
            current_generation: None,
        });
    };
    // Do not let an unauthorized caller burn the owner's one-use token.
    if record.principal != principal {
        return Err(AccessibilityPlaneError::PermissionDenied);
    }
    let record = state
        .cursors
        .remove(&token_digest)
        .ok_or(AccessibilityPlaneError::Internal)?;
    if record.expires_at <= now {
        return Err(AccessibilityPlaneError::StaleReference {
            current_generation: None,
        });
    }
    if &record.binding != binding {
        return Err(AccessibilityPlaneError::InvalidRequest);
    }
    Ok(record.continuation)
}

fn issue_cursor(
    state: &mut PlaneState,
    principal: &str,
    binding: CursorBinding,
    projection: &AccessibilityQueryProjection,
    config: AccessibilityPlaneConfig,
) -> Result<Option<AccessibilityPageCursor>, AccessibilityPlaneError> {
    let Some(continuation) = projection.continuation.clone() else {
        return Ok(None);
    };
    let now = Instant::now();
    purge_expired_cursors(state, now);
    let principal_count = state
        .cursors
        .values()
        .filter(|record| record.principal == principal)
        .count();
    if state.cursors.len() >= config.max_total_cursors
        || principal_count >= config.max_cursors_per_principal
    {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    let expires_at = now
        .checked_add(config.cursor_ttl)
        .ok_or(AccessibilityPlaneError::Internal)?;
    for _ in 0..8 {
        let token = Uuid::new_v4().simple().to_string();
        let token_digest = cursor_token_digest(&token);
        if insert_cursor_digest(
            state,
            token_digest,
            CursorRecord {
                principal: principal.to_owned(),
                expires_at,
                binding,
                continuation: continuation.clone(),
            },
        ) {
            return AccessibilityPageCursor::new(token)
                .map(Some)
                .map_err(|_| AccessibilityPlaneError::Internal);
        }
    }
    Err(AccessibilityPlaneError::Internal)
}

fn cursor_token_digest(token: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(CURSOR_TOKEN_DIGEST_DOMAIN);
    digest.update((token.len() as u64).to_be_bytes());
    digest.update(token.as_bytes());
    digest.finalize().into()
}

fn discard_page_cursor(state: &mut PlaneState, cursor: Option<&AccessibilityPageCursor>) {
    if let Some(cursor) = cursor {
        state.cursors.remove(&cursor_token_digest(cursor.as_str()));
    }
}

fn insert_cursor_digest(
    state: &mut PlaneState,
    token_digest: [u8; 32],
    record: CursorRecord,
) -> bool {
    use std::collections::hash_map::Entry;

    match state.cursors.entry(token_digest) {
        Entry::Vacant(entry) => {
            entry.insert(record);
            true
        }
        Entry::Occupied(_) => false,
    }
}

fn purge_expired_cursors(state: &mut PlaneState, now: Instant) {
    state.cursors.retain(|_, record| record.expires_at > now);
}

fn projection_to_page(
    projection: AccessibilityQueryProjection,
    order: ElementOrder,
    next_cursor: Option<AccessibilityPageCursor>,
) -> ElementListPage {
    ElementListPage {
        desktop_id: projection.desktop_id,
        desktop_generation: projection.desktop_generation,
        atspi_generation: projection.atspi_generation,
        snapshot_revision: projection.snapshot_revision,
        order,
        elements: projection.elements,
        next_cursor,
        visited_nodes: projection.visited_nodes,
        truncated: false,
        warnings: projection.warnings,
    }
}

fn wait_result(
    plane: &DaemonAccessibilityPlane,
    atspi_generation: AtspiGeneration,
    status: ElementWaitStatus,
    evaluation: AccessibilityWaitEvaluation,
    poll_fallback_used: bool,
) -> ElementWaitResult {
    ElementWaitResult {
        desktop_id: plane.desktop_id,
        desktop_generation: plane.desktop_generation,
        atspi_generation,
        status,
        evaluated_revision: evaluation.evaluated_revision,
        predicate_satisfied: status == ElementWaitStatus::Matched,
        matched_count: evaluation.satisfying_count,
        elements: evaluation.satisfying_elements,
        poll_fallback_used,
        truncated: false,
        warnings: evaluation.warnings,
    }
}

fn resync_wait_result(
    plane: &DaemonAccessibilityPlane,
    state: &PlaneState,
    poll_fallback_used: bool,
) -> ElementWaitResult {
    ElementWaitResult {
        desktop_id: plane.desktop_id,
        desktop_generation: plane.desktop_generation,
        atspi_generation: state.cache.atspi_generation(),
        status: ElementWaitStatus::ResyncRequired,
        evaluated_revision: state.cache.revision(),
        predicate_satisfied: false,
        matched_count: 0,
        elements: Vec::new(),
        poll_fallback_used,
        truncated: false,
        warnings: Vec::new(),
    }
}

fn changed(
    state: &mut PlaneState,
    sender: &watch::Sender<ChangeSignal>,
    kind: AccessibilityIngestKind,
) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
    state.cursors.clear();
    let event = AccessibilityIngestEvent {
        desktop_id: state.desktop_id,
        desktop_generation: state.desktop_generation,
        kind,
        atspi_generation: state.cache.atspi_generation(),
        revision: state.cache.revision(),
        cache_sequence: event_cache_sequence(state),
        sources: Vec::new(),
    };
    sender.send_replace(ChangeSignal {
        atspi_generation: event.atspi_generation,
        revision: event.revision,
    });
    Ok(event)
}

fn changed_with_sources(
    state: &mut PlaneState,
    sender: &watch::Sender<ChangeSignal>,
    kind: AccessibilityIngestKind,
    sources: Vec<AccessibilityIngestSource>,
) -> Result<AccessibilityIngestEvent, AccessibilityPlaneError> {
    let mut event = changed(state, sender, kind)?;
    event.sources = sources;
    Ok(event)
}

fn event_cache_sequence(state: &PlaneState) -> u64 {
    state
        .elements
        .values()
        .map(|element| element.cache_sequence)
        .max()
        .unwrap_or(1)
}

fn resolved_ingest_source(
    state: &PlaneState,
    raw: &ObjectAddress,
) -> Result<AccessibilityIngestSource, AccessibilityPlaneError> {
    let element =
        state
            .elements
            .get(raw)
            .cloned()
            .ok_or(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            })?;
    let snapshot = state
        .cache
        .resolve_exact(&element)
        .map_err(map_model_error)?;
    let bounds = snapshot
        .component
        .as_ref()
        .and_then(|component| component.extents);
    let value = snapshot.value.as_ref().map(|value| value.current);
    let caret_offset = snapshot
        .text
        .as_ref()
        .and_then(|text| u32::try_from(text.caret_offset).ok());
    let text_selection = snapshot.text.as_ref().and_then(|text| {
        let selection = text.selections.first()?;
        let start = u32::try_from(selection.start).ok()?;
        let end = u32::try_from(selection.end).ok()?;
        Some((start, end.saturating_sub(start)))
    });
    Ok(AccessibilityIngestSource {
        raw: raw.clone(),
        element: Some(element),
        metadata: AccessibilityEventMetadata {
            bounds,
            value,
            caret_offset,
            text_selection,
        },
    })
}

fn validate_cache_page(
    page: &CachePage,
    config: AccessibilityPlaneConfig,
) -> Result<usize, AccessibilityPlaneError> {
    let byte_limit = bootstrap_byte_limit(config);
    if page.estimated_bytes > byte_limit
        || page.nodes.len() > config.model_limits.max_live_nodes
        || page.nodes.len() > config.raw_cache_limits.max_nodes
    {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    if page.accessibility_generation == 0
        || page
            .nodes
            .windows(2)
            .any(|pair| pair[0].item.object >= pair[1].item.object)
        || page.after.as_ref().is_some_and(|after| {
            page.nodes
                .first()
                .is_none_or(|first| &first.item.object <= after)
        })
        || page.next_after.as_ref().is_some_and(|after| {
            page.nodes
                .last()
                .is_none_or(|last| &last.item.object != after)
        })
    {
        return Err(AccessibilityPlaneError::InvalidRequest);
    }
    let mut actual_bytes = 0_usize;
    for node in &page.nodes {
        let node_bytes = validate_cached_node(node, page.revision, config.raw_cache_limits)?;
        actual_bytes = actual_bytes
            .checked_add(node_bytes)
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    if actual_bytes > byte_limit {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    if actual_bytes != page.estimated_bytes {
        return Err(AccessibilityPlaneError::InvalidRequest);
    }
    Ok(actual_bytes)
}

fn bootstrap_byte_limit(config: AccessibilityPlaneConfig) -> usize {
    config
        .max_bootstrap_bytes
        .min(config.raw_cache_limits.max_bootstrap_bytes)
        .min(config.raw_cache_limits.max_total_bytes)
}

fn validate_cached_node(
    node: &CachedNode,
    maximum_revision: u64,
    limits: CacheLimits,
) -> Result<usize, AccessibilityPlaneError> {
    if node.application_generation == 0
        || node.revision == 0
        || node.revision > maximum_revision
        || node.item.object.bus_name() != node.item.application.bus_name()
        || node.identity_fingerprint != node.item.identity_fingerprint()
    {
        return Err(AccessibilityPlaneError::InvalidRequest);
    }
    let item = &node.item;
    if item.name.len() > limits.max_string_bytes
        || item.description.len() > limits.max_string_bytes
        || item.interfaces.len() > limits.max_interfaces
        || item.states.len() > limits.max_states
        || item.legacy_children.len() > limits.max_children
        || item
            .interfaces
            .iter()
            .any(|interface| interface.len() > limits.max_string_bytes)
    {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    let bytes = checked_raw_node_bytes(node)?;
    if bytes > limits.max_item_bytes {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    Ok(bytes)
}

fn checked_raw_node_bytes(node: &CachedNode) -> Result<usize, AccessibilityPlaneError> {
    let item = &node.item;
    let mut bytes = std::mem::size_of::<CachedNode>();
    for value in [
        item.object.bus_name(),
        item.object.object_path(),
        item.application.bus_name(),
        item.application.object_path(),
        &item.name,
        &item.description,
    ] {
        bytes = bytes
            .checked_add(value.len())
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    bytes = bytes
        .checked_add(
            item.states
                .len()
                .checked_mul(std::mem::size_of::<u32>())
                .ok_or(AccessibilityPlaneError::ResourceExhausted)?,
        )
        .and_then(|bytes| {
            bytes.checked_add(
                item.interfaces
                    .len()
                    .checked_mul(std::mem::size_of::<String>())?,
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(
                item.legacy_children
                    .len()
                    .checked_mul(std::mem::size_of::<ObjectAddress>())?,
            )
        })
        .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    if let Some(parent) = &item.parent {
        bytes = bytes
            .checked_add(parent.bus_name().len())
            .and_then(|bytes| bytes.checked_add(parent.object_path().len()))
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    for interface in &item.interfaces {
        bytes = bytes
            .checked_add(interface.len())
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    for child in &item.legacy_children {
        bytes = bytes
            .checked_add(child.bus_name().len())
            .and_then(|bytes| bytes.checked_add(child.object_path().len()))
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    if let Some(text) = &node.live.text {
        bytes = bytes
            .checked_add(
                text.selections
                    .len()
                    .checked_mul(std::mem::size_of::<xenoteer_atspi::SelectionRangeEvidence>())
                    .ok_or(AccessibilityPlaneError::ResourceExhausted)?,
            )
            .ok_or(AccessibilityPlaneError::ResourceExhausted)?;
    }
    Ok(bytes)
}

fn rebuild_all(state: &mut PlaneState) -> Result<(), AccessibilityPlaneError> {
    let addresses = state.raw_nodes.keys().cloned().collect::<Vec<_>>();
    // Births must be observed in deterministic cache order because the core
    // owns a global sequence fence.
    for address in &addresses {
        upsert_address(state, address)?;
    }
    // Resolve parent references after every birth exists.
    for address in &addresses {
        upsert_address(state, address)?;
    }
    Ok(())
}

fn refresh_all_normalized_snapshots(state: &mut PlaneState) -> Result<(), AccessibilityPlaneError> {
    let addresses = state.raw_nodes.keys().cloned().collect::<Vec<_>>();
    for address in addresses {
        upsert_address(state, &address)?;
    }
    Ok(())
}

fn prospective_raw_upsert_bytes(
    state: &PlaneState,
    node: &CachedNode,
    node_bytes: usize,
    maximum_total_bytes: usize,
) -> Result<usize, AccessibilityPlaneError> {
    let predecessor_bytes = state
        .raw_nodes
        .get(&node.item.object)
        .map(checked_raw_node_bytes)
        .transpose()?
        .unwrap_or(0);
    let total_bytes = state
        .raw_estimated_bytes
        .checked_sub(predecessor_bytes)
        .and_then(|bytes| bytes.checked_add(node_bytes))
        .ok_or(AccessibilityPlaneError::Internal)?;
    if total_bytes > maximum_total_bytes {
        return Err(AccessibilityPlaneError::ResourceExhausted);
    }
    Ok(total_bytes)
}

fn apply_upsert(
    state: &mut PlaneState,
    node: CachedNode,
    total_bytes: usize,
) -> Result<(), AccessibilityPlaneError> {
    let address = node.item.object.clone();
    // AddAccessible represents a new object birth even when a toolkit reuses
    // the same path and publishes identical fields.
    if let Some(predecessor) = state.elements.remove(&address) {
        state.cache.remove(&predecessor).map_err(map_model_error)?;
    }
    state.correlations.remove(&address);
    state.raw_nodes.insert(address.clone(), node);
    state.raw_estimated_bytes = total_bytes;
    upsert_address(state, &address)?;
    // A previously orphaned child becomes coherent when its parent arrives;
    // refresh only those direct dependants.
    let children = state
        .raw_nodes
        .iter()
        .filter(|(_, node)| node.item.parent.as_ref() == Some(&address))
        .map(|(address, _)| address.clone())
        .collect::<Vec<_>>();
    for child in children {
        upsert_address(state, &child)?;
    }
    Ok(())
}

fn apply_refresh(
    state: &mut PlaneState,
    node: CachedNode,
    total_bytes: usize,
) -> Result<(), AccessibilityPlaneError> {
    let address = node.item.object.clone();
    let predecessor =
        state
            .raw_nodes
            .get(&address)
            .ok_or(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            })?;
    let element = state
        .elements
        .get(&address)
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    state
        .cache
        .resolve_exact(element)
        .map_err(map_model_error)?;
    let actor_application_generation = state
        .actor_application_generations
        .get(node.item.application.bus_name())
        .copied()
        .ok_or(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        })?;
    if predecessor.item.object != node.item.object
        || predecessor.item.application != node.item.application
        || predecessor.application_generation != node.application_generation
        || actor_application_generation != node.application_generation
    {
        return Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        });
    }
    let mut ancestor = node.item.parent.clone();
    let mut visited = BTreeSet::new();
    while let Some(parent) = ancestor {
        if parent == address || !visited.insert(parent.clone()) {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            });
        }
        let parent_node =
            state
                .raw_nodes
                .get(&parent)
                .ok_or(AccessibilityPlaneError::ResyncRequired {
                    current_generation: None,
                })?;
        if parent_node.item.application != node.item.application
            || !state.elements.contains_key(&parent)
        {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            });
        }
        ancestor = parent_node.item.parent.clone();
    }

    let invalidates_correlation =
        is_correlation_target(predecessor) || is_correlation_target(&node);
    state.raw_nodes.insert(address.clone(), node);
    state.raw_estimated_bytes = total_bytes;
    if invalidates_correlation {
        state.correlations.remove(&address);
        refresh_all_normalized_snapshots(state)
    } else {
        // `upsert_address` sees the existing exact ElementRef and refreshes the
        // core snapshot without consuming a new public birth sequence.
        upsert_address(state, &address)
    }
}

fn apply_removed(
    state: &mut PlaneState,
    addresses: Vec<ObjectAddress>,
) -> Result<(), AccessibilityPlaneError> {
    for address in addresses {
        if let Some(node) = state.raw_nodes.remove(&address) {
            let node_bytes = checked_raw_node_bytes(&node)?;
            state.raw_estimated_bytes = state
                .raw_estimated_bytes
                .checked_sub(node_bytes)
                .ok_or(AccessibilityPlaneError::Internal)?;
        }
        if let Some(element) = state.elements.remove(&address) {
            state.cache.remove(&element).map_err(map_model_error)?;
        }
        state.correlations.remove(&address);
    }
    Ok(())
}

fn apply_application_invalidation(
    state: &mut PlaneState,
    bus_name: &str,
) -> Result<(), AccessibilityPlaneError> {
    let removed_bytes = state
        .raw_nodes
        .iter()
        .filter(|(address, _)| address.bus_name() == bus_name)
        .try_fold(0_usize, |bytes, (_, node)| {
            bytes
                .checked_add(checked_raw_node_bytes(node)?)
                .ok_or(AccessibilityPlaneError::Internal)
        })?;
    let applications = state
        .applications
        .iter()
        .filter(|(address, _)| address.bus_name() == bus_name)
        .map(|(_, application)| application.clone())
        .collect::<Vec<_>>();
    for application in applications {
        state
            .cache
            .remove_application(&application)
            .map_err(map_model_error)?;
    }
    state
        .raw_nodes
        .retain(|address, _| address.bus_name() != bus_name);
    state.raw_estimated_bytes = state
        .raw_estimated_bytes
        .checked_sub(removed_bytes)
        .ok_or(AccessibilityPlaneError::Internal)?;
    state
        .elements
        .retain(|address, _| address.bus_name() != bus_name);
    state
        .correlations
        .retain(|address, _| address.bus_name() != bus_name);
    state
        .applications
        .retain(|address, _| address.bus_name() != bus_name);
    Ok(())
}

fn validate_authoritative_topology(
    nodes: &BTreeMap<ObjectAddress, CachedNode>,
) -> Result<(), AccessibilityPlaneError> {
    for node in nodes.values() {
        let Some(parent) = node.item.parent.as_ref() else {
            continue;
        };
        let parent_node = nodes
            .get(parent)
            .ok_or(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            })?;
        if parent == &node.item.object || parent_node.item.application != node.item.application {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            });
        }
    }
    Ok(())
}

fn upsert_address(
    state: &mut PlaneState,
    address: &ObjectAddress,
) -> Result<(), AccessibilityPlaneError> {
    let node = state
        .raw_nodes
        .get(address)
        .cloned()
        .ok_or(AccessibilityPlaneError::NotFound)?;
    let application = ensure_application(state, &node)?;
    let element = match state.elements.get(address) {
        Some(element) if element.application == application => element.clone(),
        Some(_) => {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            });
        }
        None => {
            let element = state
                .cache
                .next_element_ref(
                    &application,
                    AtspiObjectPath::new(address.object_path())
                        .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
                    identity_hash(&[
                        b"object-v1",
                        address.bus_name().as_bytes(),
                        address.object_path().as_bytes(),
                        &node.application_generation.to_be_bytes(),
                    ])?,
                )
                .map_err(map_model_error)?;
            state.elements.insert(address.clone(), element.clone());
            element
        }
    };
    let snapshot = normalized_snapshot(state, &node, element)?;
    state.cache.observe(snapshot).map_err(map_model_error)?;
    Ok(())
}

fn ensure_application(
    state: &mut PlaneState,
    node: &CachedNode,
) -> Result<ApplicationRef, AccessibilityPlaneError> {
    match state
        .actor_application_generations
        .get(node.item.application.bus_name())
    {
        Some(generation) if *generation != node.application_generation => {
            return Err(AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            });
        }
        Some(_) => {}
        None => {
            state.actor_application_generations.insert(
                node.item.application.bus_name().to_owned(),
                node.application_generation,
            );
        }
    }
    let expected_hash = identity_hash(&[
        b"application-v1",
        node.item.application.bus_name().as_bytes(),
        node.item.application.object_path().as_bytes(),
        &node.application_generation.to_be_bytes(),
    ])?;
    if let Some(existing) = state.applications.get(&node.item.application) {
        if existing.identity_hash == expected_hash {
            return Ok(existing.clone());
        }
        return Err(AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        });
    }
    let application = state
        .cache
        .register_application(
            AtspiBusName::new(node.item.application.bus_name())
                .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
            AtspiObjectPath::new(node.item.application.object_path())
                .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
            expected_hash,
        )
        .map_err(map_model_error)?;
    state
        .applications
        .insert(node.item.application.clone(), application.clone());
    Ok(application)
}

fn normalized_snapshot(
    state: &PlaneState,
    node: &CachedNode,
    element: ElementRef,
) -> Result<ElementSnapshot, AccessibilityPlaneError> {
    let parent = node
        .item
        .parent
        .as_ref()
        .and_then(|address| state.elements.get(address))
        .cloned();
    let role = map_role(node.item.role);
    let protected = role == ElementRole::PasswordText
        || !matches!(node.item.text_protection, TextProtection::Unprotected);
    let mut states = mapped_states(&node.item.states);
    if protected {
        states.push(ElementState::Protected);
    }
    states.sort_unstable();
    states.dedup();
    let mut interfaces = node
        .item
        .interfaces
        .iter()
        .filter_map(|interface| map_interface(interface))
        .collect::<Vec<_>>();
    interfaces.sort_unstable();
    interfaces.dedup();
    let value = node.live.value.map(|value| ElementValueSnapshot {
        current: value.current,
        minimum: Some(value.minimum),
        maximum: Some(value.maximum),
        increment: Some(value.minimum_increment),
        text: None,
    });
    let text = if node.item.text_protection == TextProtection::Unknown {
        // Future roles fail closed: absence must not become fabricated safe
        // metadata until their protection semantics have been reviewed.
        None
    } else {
        node.live
            .text
            .as_ref()
            .map(|text| {
                let character_count = text
                    .character_count
                    .ok_or(AccessibilityPlaneError::Internal)
                    .and_then(|value| {
                        i32::try_from(value).map_err(|_| AccessibilityPlaneError::Internal)
                    })?;
                let caret_offset = text.caret_offset.ok_or(AccessibilityPlaneError::Internal)?;
                let selections = text
                    .selections
                    .iter()
                    .map(|range| {
                        Ok(ElementTextRange {
                            start: i32::try_from(range.start)
                                .map_err(|_| AccessibilityPlaneError::Internal)?,
                            end: i32::try_from(range.end)
                                .map_err(|_| AccessibilityPlaneError::Internal)?,
                        })
                    })
                    .collect::<Result<Vec<_>, AccessibilityPlaneError>>()?;
                Ok(ElementTextSnapshot {
                    character_count,
                    caret_offset,
                    selections,
                    content: None,
                    content_truncated: false,
                    protected,
                })
            })
            .transpose()?
    };
    let component = node
        .live
        .bounds
        .map(|bounds| {
            let width =
                u32::try_from(bounds.width).map_err(|_| AccessibilityPlaneError::Internal)?;
            let height =
                u32::try_from(bounds.height).map_err(|_| AccessibilityPlaneError::Internal)?;
            Ok(ElementComponentSnapshot {
                coordinate_space: CoordinateSpace::AtspiScreen,
                // AT-SPI permits empty extents for hidden/offscreen objects;
                // the public Rect contract is non-empty, so preserve the
                // Component evidence while representing its geometry as absent.
                extents: if width == 0 || height == 0 {
                    None
                } else {
                    Some(
                        Rect::new(bounds.x, bounds.y, width, height)
                            .map_err(|_| AccessibilityPlaneError::Internal)?,
                    )
                },
                layer: None,
                z_order: None,
                alpha: None,
            })
        })
        .transpose()?;
    let window_correlation = inherited_window_correlation(state, node);
    Ok(ElementSnapshot {
        element,
        parent,
        index_in_parent: node
            .item
            .index_in_parent
            .map(u32::try_from)
            .transpose()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
        child_count: node
            .item
            .child_count
            .map(u32::try_from)
            .transpose()
            .map_err(|_| AccessibilityPlaneError::InvalidRequest)?,
        role: ElementRoleSnapshot {
            role,
            raw_name: None,
            raw_numeric: Some(node.item.role),
        },
        name: if protected {
            None
        } else {
            nonempty(node.item.name.clone())
        },
        description: if protected {
            None
        } else {
            nonempty(node.item.description.clone())
        },
        accessible_id: None,
        locale: None,
        states,
        interfaces,
        actions: Vec::new(),
        value,
        text,
        component,
        attributes: Vec::new(),
        relations: Vec::new(),
        window_correlation,
        revision: state.cache.revision(),
        completeness: ElementCompleteness::Partial,
        truncated: false,
        warnings: Vec::new(),
    })
}

fn inherited_window_correlation(state: &PlaneState, node: &CachedNode) -> ElementWindowCorrelation {
    let mut current = Some(node.item.object.clone());
    let mut visited = BTreeSet::new();
    while let Some(address) = current {
        if !visited.insert(address.clone()) {
            break;
        }
        if let Some(installed) = state.correlations.get(&address)
            && state.elements.get(&address) == Some(&installed.evidence.current_element)
        {
            return installed.correlation.clone();
        }
        current = state
            .raw_nodes
            .get(&address)
            .and_then(|current_node| current_node.item.parent.clone());
    }
    ElementWindowCorrelation {
        window: None,
        confidence: WindowCorrelationConfidence::None,
        evidence: Vec::new(),
        conflicting_evidence: false,
    }
}

fn is_correlation_target(node: &CachedNode) -> bool {
    matches!(
        map_role(node.item.role),
        ElementRole::Application | ElementRole::Window | ElementRole::Dialog
    )
}

fn identity_hash(parts: &[&[u8]]) -> Result<AccessibilityIdentityHash, AccessibilityPlaneError> {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").map_err(|_| AccessibilityPlaneError::Internal)?;
    }
    AccessibilityIdentityHash::new(encoded).map_err(|_| AccessibilityPlaneError::Internal)
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn map_role(role: u32) -> ElementRole {
    match role {
        2 => ElementRole::Alert,
        6 => ElementRole::Canvas,
        7 => ElementRole::CheckBox,
        11 => ElementRole::ComboBox,
        16 => ElementRole::Dialog,
        23 | 69 => ElementRole::Window,
        27 => ElementRole::Image,
        29 => ElementRole::Label,
        31 | 98 => ElementRole::List,
        32 => ElementRole::ListItem,
        33 | 34 | 41 => ElementRole::Menu,
        8 | 35 | 45 | 59 | 129 => ElementRole::MenuItem,
        37 => ElementRole::Tab,
        38 => ElementRole::TabList,
        39 => ElementRole::Panel,
        40 => ElementRole::PasswordText,
        42 => ElementRole::ProgressBar,
        43 => ElementRole::Button,
        44 => ElementRole::RadioButton,
        48 => ElementRole::ScrollBar,
        51 => ElementRole::Slider,
        52 => ElementRole::SpinButton,
        55 | 66 | 90 | 92 => ElementRole::Table,
        56 => ElementRole::TableCell,
        60 => ElementRole::Terminal,
        61 => ElementRole::Text,
        62 => ElementRole::ToggleButton,
        63 => ElementRole::Toolbar,
        65 => ElementRole::Tree,
        75 => ElementRole::Application,
        79 => ElementRole::Entry,
        82 | 93..=96 => ElementRole::Document,
        85 => ElementRole::Section,
        88 => ElementRole::Link,
        91 => ElementRole::TreeItem,
        _ => ElementRole::Unknown,
    }
}

fn map_state(state: u32) -> Option<ElementState> {
    Some(match state {
        1 => ElementState::Active,
        3 => ElementState::Busy,
        4 => ElementState::Checked,
        6 => ElementState::Defunct,
        7 => ElementState::Editable,
        8 => ElementState::Enabled,
        9 => ElementState::Expandable,
        10 => ElementState::Expanded,
        11 => ElementState::Focusable,
        12 => ElementState::Focused,
        16 => ElementState::Modal,
        18 => ElementState::MultiSelectable,
        19 => ElementState::Opaque,
        20 => ElementState::Pressed,
        22 => ElementState::Selectable,
        23 => ElementState::Selected,
        24 => ElementState::Sensitive,
        25 => ElementState::Showing,
        26 => ElementState::SingleLine,
        27 => ElementState::Stale,
        28 => ElementState::Transient,
        30 => ElementState::Visible,
        32 => ElementState::Indeterminate,
        33 => ElementState::Required,
        40 => ElementState::Visited,
        43 => ElementState::ReadOnly,
        _ => return None,
    })
}

fn raw_state_contains(words: &[u32], state: u32) -> bool {
    let state = state as usize;
    words
        .get(state / u32::BITS as usize)
        .is_some_and(|word| word & (1_u32 << (state % u32::BITS as usize)) != 0)
}

fn mapped_states(words: &[u32]) -> Vec<ElementState> {
    words
        .iter()
        .enumerate()
        .flat_map(|(word_index, word)| {
            (0..u32::BITS).filter_map(move |bit| {
                if word & (1_u32 << bit) == 0 {
                    return None;
                }
                word_index
                    .checked_mul(u32::BITS as usize)?
                    .checked_add(bit as usize)
                    .and_then(|state| u32::try_from(state).ok())
                    .and_then(map_state)
            })
        })
        .collect()
}

fn map_interface(interface: &str) -> Option<ElementInterface> {
    Some(match interface.rsplit('.').next().unwrap_or(interface) {
        "Accessible" => ElementInterface::Accessible,
        "Action" => ElementInterface::Action,
        "Application" => ElementInterface::Application,
        "Collection" => ElementInterface::Collection,
        "Component" => ElementInterface::Component,
        "Document" => ElementInterface::Document,
        "EditableText" => ElementInterface::EditableText,
        "Hypertext" => ElementInterface::Hypertext,
        "Image" => ElementInterface::Image,
        "Selection" => ElementInterface::Selection,
        "Table" => ElementInterface::Table,
        "TableCell" => ElementInterface::TableCell,
        "Text" => ElementInterface::Text,
        "Value" => ElementInterface::Value,
        _ => return None,
    })
}

fn force_resync(state: &mut PlaneState) {
    state.source_revision = None;
    state.resync_pending = true;
    state.bootstrap = None;
    state.raw_nodes.clear();
    state.raw_estimated_bytes = 0;
    state.applications.clear();
    state.actor_application_generations.clear();
    state.elements.clear();
    state.correlations.clear();
    state.cursors.clear();
    state.expected_actor_generation = None;
}

fn map_model_error(error: AccessibilityModelError) -> AccessibilityPlaneError {
    match error {
        AccessibilityModelError::InvalidLimits
        | AccessibilityModelError::NilIdentifier
        | AccessibilityModelError::InvalidApplication(_)
        | AccessibilityModelError::InvalidSnapshot(_)
        | AccessibilityModelError::InvalidReference(_)
        | AccessibilityModelError::UnexpectedCacheSequence => {
            AccessibilityPlaneError::InvalidRequest
        }
        AccessibilityModelError::StaleGeneration
        | AccessibilityModelError::StaleApplication
        | AccessibilityModelError::StaleReference
        | AccessibilityModelError::AlreadyRemoved => AccessibilityPlaneError::StaleReference {
            current_generation: None,
        },
        AccessibilityModelError::ApplicationNotFound | AccessibilityModelError::NotFound => {
            AccessibilityPlaneError::NotFound
        }
        AccessibilityModelError::ResyncRequired(_) => AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        },
        AccessibilityModelError::GenerationExhausted
        | AccessibilityModelError::SequenceExhausted
        | AccessibilityModelError::RevisionExhausted => AccessibilityPlaneError::Internal,
    }
}

fn map_query_error(error: AccessibilityQueryError) -> AccessibilityPlaneError {
    match error {
        AccessibilityQueryError::InvalidRequest(_)
        | AccessibilityQueryError::RegexBuild
        | AccessibilityQueryError::InvalidPageLimit
        | AccessibilityQueryError::Offset
        | AccessibilityQueryError::FutureRevision => AccessibilityPlaneError::InvalidRequest,
        AccessibilityQueryError::Fingerprint => AccessibilityPlaneError::Internal,
        AccessibilityQueryError::LimitExceeded(_) => AccessibilityPlaneError::QueryLimitExceeded,
        AccessibilityQueryError::ContinuationMismatch | AccessibilityQueryError::StaleReference => {
            AccessibilityPlaneError::StaleReference {
                current_generation: None,
            }
        }
        AccessibilityQueryError::NoMatch => AccessibilityPlaneError::NotFound,
        AccessibilityQueryError::Ambiguous { .. } => AccessibilityPlaneError::AmbiguousTarget,
        AccessibilityQueryError::ResyncRequired => AccessibilityPlaneError::ResyncRequired {
            current_generation: None,
        },
    }
}

#[cfg(test)]
#[path = "accessibility_plane_tests.rs"]
mod tests;
