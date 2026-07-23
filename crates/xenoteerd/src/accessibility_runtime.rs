//! Nonfatal daemon composition for the bounded AT-SPI actor and read mirror.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;
use xenoteer_atspi::{
    AtspiActorConfig, AtspiActorError, AtspiActorEvent, AtspiActorExit, AtspiActorJoin,
    AtspiActorSpawnError, AtspiActorState, AtspiBackendConnector, AtspiEventReceiver, AtspiHandle,
    AtspiTryRecv, CacheLimits, CacheMutation, CacheMutationDetail, CacheMutationKind,
    LiveAtspiConnector, ObjectAddress, SemanticError, spawn_atspi_actor,
};
use xenoteer_core::{AccessibilityConfig, AccessibilityModelLimits};
#[cfg(test)]
use xenoteer_protocol::NormalizedEvent;
use xenoteer_protocol::{
    ACCESSIBILITY_CURSOR_TTL_MS, AccessibilityResyncReason, AtspiGeneration, DesktopGeneration,
    DesktopId, MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL,
};
use xenoteer_server::AccessibilityPlaneError;

use crate::accessibility_plane::{
    AccessibilityActionTargetEvidence, AccessibilityIngestKind, AccessibilityPlaneConfig,
    AccessibilityPollDispatch, AccessibilityPollReconciler, AccessibilityResyncCause,
    DaemonAccessibilityPlane,
};
#[cfg(test)]
use crate::observation_plane::WindowEventSinkError;
use crate::{
    accessibility_events::AccessibilityEventPublisher, observation_plane::WindowEventSink,
};

/// Actor and mirror evidence used by the synchronous capability projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccessibilityRuntimeSnapshot {
    pub(crate) actor_state: AtspiActorState,
    pub(crate) mirror_ready: bool,
    pub(crate) accessibility_generation: u64,
    pub(crate) cache_revision: u64,
}

/// Cheap cloneable reader for current AT-SPI and mirror state.
#[derive(Clone, Debug)]
pub(crate) struct AccessibilityRuntimeReader {
    state: watch::Receiver<AccessibilityRuntimeSnapshot>,
}

impl AccessibilityRuntimeReader {
    #[must_use]
    pub(crate) fn snapshot(&self) -> AccessibilityRuntimeSnapshot {
        *self.state.borrow()
    }
}

/// Fully mapped and bounded runtime configuration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AccessibilityRuntimeConfig {
    enabled: bool,
    pub(crate) actor: AtspiActorConfig,
    plane: AccessibilityPlaneConfig,
    pub(crate) requested_event_capacity: usize,
    /// Aggregate transport cardinality across the five filtered zbus streams.
    pub(crate) raw_signal_queue_capacity: usize,
    /// Per-layer decoded cardinality used by the bootstrap, actor, and mirror.
    /// Each layer owns a separate queue; this is not an aggregate memory budget.
    pub(crate) decoded_event_capacity: usize,
    pub(crate) maintenance_interval: Duration,
    pub(crate) inter_page_delay: Duration,
}

impl AccessibilityRuntimeConfig {
    /// Map the validated public configuration without inheriting ambient queue defaults.
    pub(crate) fn from_config(config: &AccessibilityConfig) -> Self {
        let mut cache_limits = CacheLimits::default();
        cache_limits.max_nodes = config.max_cached_nodes();
        // The mirror cannot install more than one bounded bootstrap. Keeping the
        // live cache at that same ceiling avoids retaining data the read plane
        // could never publish atomically.
        cache_limits.max_total_bytes = cache_limits
            .max_total_bytes
            .min(cache_limits.max_bootstrap_bytes);
        let byte_weighted_queue_ceiling = cache_limits
            .max_total_bytes
            .checked_div(cache_limits.max_item_bytes)
            .unwrap_or(0)
            .max(1);
        let request_capacity =
            effective_queue_capacity(config.request_capacity(), byte_weighted_queue_ceiling);
        let event_capacity =
            effective_queue_capacity(config.event_capacity(), byte_weighted_queue_ceiling);
        let proxy_timeout = Duration::from_millis(config.proxy_timeout_ms());
        let query_timeout = Duration::from_millis(config.query_timeout_ms());
        let actor = AtspiActorConfig {
            request_capacity,
            backend_event_capacity: event_capacity,
            event_capacity,
            read_page_nodes: config
                .max_snapshot_nodes()
                .min(config.max_cached_nodes())
                .max(1),
            read_page_bytes: config
                .max_snapshot_bytes()
                .min(cache_limits.max_total_bytes)
                .max(1),
            connect_timeout: proxy_timeout,
            bootstrap_timeout: query_timeout,
            proxy_call_timeout: proxy_timeout,
            shutdown_timeout: proxy_timeout,
            reconnect_initial: Duration::from_millis(config.reconnect_initial_backoff_ms()),
            reconnect_max: Duration::from_millis(config.reconnect_max_backoff_ms()),
            cache_limits,
        };
        let plane = AccessibilityPlaneConfig {
            model_limits: AccessibilityModelLimits {
                max_live_nodes: config.max_cached_nodes(),
                max_tombstones: config.max_tombstones(),
            },
            raw_cache_limits: cache_limits,
            cursor_ttl: Duration::from_millis(
                config
                    .cursor_ttl_ms()
                    .min(u64::from(ACCESSIBILITY_CURSOR_TTL_MS)),
            ),
            max_total_cursors: config.token_capacity(),
            max_cursors_per_principal: MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL
                .min(config.token_capacity()),
            max_pending_waits: config.max_waiters(),
            max_pending_reads: request_capacity,
            max_bootstrap_bytes: cache_limits.max_total_bytes,
            max_nodes_per_query: u32::try_from(config.max_nodes_per_query()).unwrap_or(0),
            max_selector_depth: u16::try_from(config.max_selector_depth()).unwrap_or(0),
            max_query_matches: u16::try_from(config.max_query_matches()).unwrap_or(0),
            query_timeout_ms: u32::try_from(config.query_timeout_ms()).unwrap_or(0),
            max_snapshot_nodes: u32::try_from(config.max_snapshot_nodes()).unwrap_or(0),
            max_snapshot_bytes: u32::try_from(config.max_snapshot_bytes()).unwrap_or(0),
        };
        Self {
            enabled: config.enabled(),
            actor,
            plane,
            requested_event_capacity: config.event_capacity(),
            raw_signal_queue_capacity: LiveAtspiConnector::MIN_RAW_SIGNAL_QUEUE_CAPACITY,
            decoded_event_capacity: event_capacity,
            maintenance_interval: Duration::from_millis(250)
                .max(Duration::from_millis(config.reconnect_initial_backoff_ms())),
            inter_page_delay: Duration::ZERO,
        }
    }

    #[cfg(test)]
    pub(crate) const fn plane_config(&self) -> AccessibilityPlaneConfig {
        self.plane
    }
}

fn effective_queue_capacity(requested: usize, byte_weighted_ceiling: usize) -> usize {
    requested.min(byte_weighted_ceiling).max(1)
}

#[cfg(test)]
struct DisabledAccessibilityEventSink;

#[cfg(test)]
impl WindowEventSink for DisabledAccessibilityEventSink {
    fn enabled(&self) -> bool {
        false
    }

    fn try_emit(&self, _event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        Ok(())
    }
}

/// Owns cancellation and both actor/mirror task sets.
pub(crate) struct AccessibilityRuntime {
    plane: Arc<DaemonAccessibilityPlane>,
    semantic: AccessibilitySemanticRuntime,
    reader: AccessibilityRuntimeReader,
    cancellation: CancellationToken,
    mirror_join: Option<JoinHandle<()>>,
    actor_join: Option<AtspiActorJoin>,
    shutdown_timeout: Duration,
}

impl AccessibilityRuntime {
    #[must_use]
    pub(crate) fn plane(&self) -> Arc<DaemonAccessibilityPlane> {
        Arc::clone(&self.plane)
    }

    #[must_use]
    pub(crate) fn reader(&self) -> AccessibilityRuntimeReader {
        self.reader.clone()
    }

    /// Cloneable exact semantic-action authority retained outside the mirror task.
    #[must_use]
    pub(crate) fn semantic_runtime(&self) -> AccessibilitySemanticRuntime {
        self.semantic.clone()
    }

    #[must_use]
    pub(crate) fn shutdown_handle(&self) -> AccessibilityRuntimeShutdown {
        AccessibilityRuntimeShutdown {
            cancellation: self.cancellation.clone(),
        }
    }

    /// Cancel and join without allowing an optional backend to hang daemon shutdown.
    pub(crate) async fn shutdown(mut self) -> AccessibilityRuntimeExit {
        self.cancellation.cancel();
        let actor_exit = if let Some(actor_join) = self.actor_join.take() {
            timeout(self.shutdown_timeout, actor_join.shutdown())
                .await
                .ok()
        } else {
            None
        };
        let mirror_stopped = if let Some(mut mirror_join) = self.mirror_join.take() {
            match timeout(self.shutdown_timeout, &mut mirror_join).await {
                Ok(result) => result.is_ok(),
                Err(_) => {
                    mirror_join.abort();
                    false
                }
            }
        } else {
            true
        };
        AccessibilityRuntimeExit {
            actor_exit,
            mirror_stopped,
        }
    }
}

/// Cloneable composition of the actor-owned semantic lane and its exact mirror.
#[derive(Clone)]
pub(crate) struct AccessibilitySemanticRuntime {
    handle: AtspiHandle,
    plane: Arc<DaemonAccessibilityPlane>,
}

impl AccessibilitySemanticRuntime {
    #[must_use]
    pub(crate) fn handle(&self) -> AtspiHandle {
        self.handle.clone()
    }

    #[must_use]
    pub(crate) fn plane(&self) -> Arc<DaemonAccessibilityPlane> {
        Arc::clone(&self.plane)
    }
}

impl AccessibilityPollReconciler for AccessibilitySemanticRuntime {
    fn reconcile_exact<'a>(
        &'a self,
        evidence: AccessibilityActionTargetEvidence,
        deadline: tokio::time::Instant,
    ) -> xenoteer_server::AccessibilityFuture<
        'a,
        Result<AccessibilityPollDispatch, AccessibilityPlaneError>,
    > {
        Box::pin(async move {
            let accessibility_generation = evidence.accessibility_generation();
            let source_revision = evidence.source_revision();
            let node_revision = evidence.node_revision();
            let result = self
                .handle
                .reconcile_semantic_target(
                    evidence.semantic_target_request(),
                    deadline,
                    CancellationToken::new(),
                )
                .await
                .map_err(|error| {
                    tracing::debug!(
                        error_class = reconcile_error_class(&error),
                        "exact accessibility metadata hydration failed closed"
                    );
                    map_reconcile_error(error)
                })?;
            if result.accessibility_generation != accessibility_generation
                || result.previous_cache_revision != source_revision
                || result.application_generation != evidence.application_generation()
                || result.cache_revision < result.previous_cache_revision
            {
                return Err(AccessibilityPlaneError::Internal);
            }
            Ok(AccessibilityPollDispatch {
                accessibility_generation,
                source_revision,
                node_revision,
            })
        })
    }
}

fn reconcile_error_class(error: &SemanticError) -> &'static str {
    match error {
        SemanticError::QueueFull => "queue_full",
        SemanticError::StaleAccessibilityGeneration { .. } => "stale_generation",
        SemanticError::StaleApplicationGeneration { .. } => "stale_application",
        SemanticError::StaleCacheRevision { .. } => "stale_revision",
        SemanticError::StaleIdentity => "stale_identity",
        SemanticError::InterfaceUnavailable(_) => "interface_unavailable",
        SemanticError::UnclassifiedTextDenied => "unclassified_text",
        SemanticError::ActionNotFound => "action_not_found",
        SemanticError::AmbiguousAction => "ambiguous_action",
        SemanticError::InvalidRequest(_) => "invalid_request",
        SemanticError::Stopped => "stopped",
        SemanticError::Unavailable => "unavailable",
        SemanticError::CancelledBeforeDispatch => "cancelled_before_dispatch",
        SemanticError::CancelledAfterDispatch => "cancelled_after_dispatch",
        SemanticError::DeadlineBeforeDispatch => "deadline_before_dispatch",
        SemanticError::DeadlineAfterDispatch => "deadline_after_dispatch",
        SemanticError::ReplyLostAfterAdmission => "reply_lost_after_admission",
        SemanticError::Backend(failure) | SemanticError::BackendAfterDispatch(failure) => {
            if failure.message == "targeted refresh common accessible metadata failed" {
                return "backend_targeted_common_metadata";
            }
            if failure.message == "targeted refresh Component metadata failed" {
                return "backend_targeted_component_metadata";
            }
            if failure.message == "targeted reconcile cache refresh failed" {
                return "backend_targeted_cache_refresh";
            }
            match failure.kind {
                xenoteer_atspi::BackendFailureKind::Timeout => "backend_timeout",
                xenoteer_atspi::BackendFailureKind::Connection => "backend_connection",
                xenoteer_atspi::BackendFailureKind::Protocol => "backend_protocol",
                xenoteer_atspi::BackendFailureKind::Stream => "backend_stream",
                xenoteer_atspi::BackendFailureKind::ActionNotFound => "backend_action_not_found",
                xenoteer_atspi::BackendFailureKind::AmbiguousAction => "backend_ambiguous_action",
            }
        }
        SemanticError::ReadEpochExhausted => "read_epoch_exhausted",
    }
}

fn map_reconcile_error(error: SemanticError) -> AccessibilityPlaneError {
    match error {
        SemanticError::QueueFull => AccessibilityPlaneError::ResourceExhausted,
        SemanticError::StaleAccessibilityGeneration { .. }
        | SemanticError::StaleApplicationGeneration { .. }
        | SemanticError::StaleCacheRevision { .. }
        | SemanticError::StaleIdentity => AccessibilityPlaneError::StaleReference {
            current_generation: None,
        },
        SemanticError::InterfaceUnavailable(_)
        | SemanticError::UnclassifiedTextDenied
        | SemanticError::ActionNotFound
        | SemanticError::AmbiguousAction => AccessibilityPlaneError::UnsupportedByTarget,
        SemanticError::InvalidRequest(_) => AccessibilityPlaneError::InvalidRequest,
        SemanticError::Stopped
        | SemanticError::Unavailable
        | SemanticError::CancelledBeforeDispatch
        | SemanticError::CancelledAfterDispatch
        | SemanticError::DeadlineBeforeDispatch
        | SemanticError::DeadlineAfterDispatch => AccessibilityPlaneError::CapabilityUnavailable,
        SemanticError::ReplyLostAfterAdmission
        | SemanticError::Backend(_)
        | SemanticError::BackendAfterDispatch(_)
        | SemanticError::ReadEpochExhausted => AccessibilityPlaneError::Internal,
    }
}

impl Drop for AccessibilityRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(mirror) = self.mirror_join.take() {
            mirror.abort();
        }
        // Dropping AtspiActorJoin requests actor cancellation.
        let _ = self.actor_join.take();
    }
}

/// Cloneable early-shutdown signal used by the HTTP drain path.
#[derive(Clone, Debug)]
pub(crate) struct AccessibilityRuntimeShutdown {
    cancellation: CancellationToken,
}

impl AccessibilityRuntimeShutdown {
    pub(crate) fn request(&self) {
        self.cancellation.cancel();
    }
}

/// Bounded terminal evidence; accessibility remains optional to the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccessibilityRuntimeExit {
    pub(crate) actor_exit: Option<AtspiActorExit>,
    pub(crate) mirror_stopped: bool,
}

/// Compose the production connector without attempting a synchronous bus probe.
pub(crate) fn spawn_live_accessibility_runtime(
    config: &AccessibilityConfig,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    event_sink: Arc<dyn WindowEventSink>,
) -> Result<AccessibilityRuntime, AccessibilityRuntimeError> {
    let config = AccessibilityRuntimeConfig::from_config(config);
    if config.actor.backend_event_capacity != config.decoded_event_capacity
        || config.actor.event_capacity != config.decoded_event_capacity
    {
        return Err(AccessibilityRuntimeError::InvalidConfiguration);
    }
    let connector = LiveAtspiConnector::new(
        config.raw_signal_queue_capacity,
        config.decoded_event_capacity,
    );
    let raw_signal_queue_worst_case_bytes = connector
        .raw_signal_queue_worst_case_bytes()
        .ok_or(AccessibilityRuntimeError::InvalidConfiguration)?;
    let decoded_queue_worst_case_bytes = config
        .decoded_event_capacity
        .checked_mul(config.actor.cache_limits.max_item_bytes)
        .ok_or(AccessibilityRuntimeError::InvalidConfiguration)?;
    tracing::info!(
        enabled = config.enabled,
        requested_event_capacity = config.requested_event_capacity,
        decoded_event_capacity = config.decoded_event_capacity,
        decoded_queue_worst_case_bytes,
        raw_signal_queue_capacity = config.raw_signal_queue_capacity,
        raw_signal_queue_worst_case_bytes,
        cache_bytes = config.actor.cache_limits.max_total_bytes,
        "configured per-layer bounded AT-SPI runtime"
    );
    spawn_accessibility_runtime_with_connector_and_event_sink(
        config,
        desktop_id,
        desktop_generation,
        connector,
        event_sink,
    )
}

#[cfg(test)]
pub(crate) fn spawn_accessibility_runtime_with_connector<C>(
    config: AccessibilityRuntimeConfig,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    connector: C,
) -> Result<AccessibilityRuntime, AccessibilityRuntimeError>
where
    C: AtspiBackendConnector,
{
    spawn_accessibility_runtime_with_connector_and_event_sink(
        config,
        desktop_id,
        desktop_generation,
        connector,
        Arc::new(DisabledAccessibilityEventSink),
    )
}

pub(crate) fn spawn_accessibility_runtime_with_connector_and_event_sink<C>(
    config: AccessibilityRuntimeConfig,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    connector: C,
    event_sink: Arc<dyn WindowEventSink>,
) -> Result<AccessibilityRuntime, AccessibilityRuntimeError>
where
    C: AtspiBackendConnector,
{
    let initial_generation =
        AtspiGeneration::new(1).map_err(|_| AccessibilityRuntimeError::InvalidConfiguration)?;
    let plane = Arc::new(DaemonAccessibilityPlane::new(
        desktop_id,
        desktop_generation,
        initial_generation,
        config.plane,
    )?);
    let spawned = spawn_atspi_actor(config.enabled, config.actor, connector)?;
    let initial = AccessibilityRuntimeSnapshot {
        actor_state: if config.enabled {
            AtspiActorState::Connecting
        } else {
            AtspiActorState::Disabled
        },
        mirror_ready: false,
        accessibility_generation: 1,
        cache_revision: 0,
    };
    let (status, receiver) = watch::channel(initial);
    let cancellation = CancellationToken::new();
    let event_publisher = AccessibilityEventPublisher::new(event_sink);
    let semantic = AccessibilitySemanticRuntime {
        handle: spawned.handle.clone(),
        plane: Arc::clone(&plane),
    };
    plane.set_poll_reconciler(Some(Arc::new(semantic.clone())));
    let mirror_join = tokio::spawn(run_mirror(
        config.enabled,
        spawned.handle,
        spawned.events,
        Arc::clone(&plane),
        status,
        cancellation.clone(),
        config.maintenance_interval,
        config.actor.bootstrap_timeout,
        config.inter_page_delay,
        config.decoded_event_capacity,
        event_publisher,
    ));
    Ok(AccessibilityRuntime {
        plane,
        semantic,
        reader: AccessibilityRuntimeReader { state: receiver },
        cancellation,
        mirror_join: Some(mirror_join),
        actor_join: Some(spawned.join),
        shutdown_timeout: config.actor.shutdown_timeout.saturating_mul(2),
    })
}

pub(crate) struct MirrorCursor {
    prepared_generation: u64,
    mirrored_generation: Option<u64>,
    mirrored_revision: Option<u64>,
    awaiting_generation_after_rebuild: Option<u64>,
    covered_rebuild_overflow: Option<(u64, u64)>,
    rebuild_required: bool,
}

impl MirrorCursor {
    const fn new() -> Self {
        Self {
            prepared_generation: 1,
            mirrored_generation: None,
            mirrored_revision: None,
            awaiting_generation_after_rebuild: None,
            covered_rebuild_overflow: None,
            rebuild_required: false,
        }
    }

    fn ready_for(&self, generation: u64) -> bool {
        self.mirrored_generation == Some(generation) && self.mirrored_revision.is_some()
    }

    fn invalidate(&mut self, rebuild_required: bool) {
        self.mirrored_generation = None;
        self.mirrored_revision = None;
        self.covered_rebuild_overflow = None;
        self.rebuild_required |= rebuild_required;
    }

    #[cfg(test)]
    pub(crate) const fn mirrored_for_test(generation: u64, revision: u64) -> Self {
        Self {
            prepared_generation: generation,
            mirrored_generation: Some(generation),
            mirrored_revision: Some(revision),
            awaiting_generation_after_rebuild: None,
            covered_rebuild_overflow: None,
            rebuild_required: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_for_test(&mut self, rebuild_required: bool) {
        self.invalidate(rebuild_required);
    }

    #[cfg(test)]
    pub(crate) fn rebuild_pending_for_test(&self) -> bool {
        self.rebuild_required || self.awaiting_generation_after_rebuild.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_mirror(
    enabled: bool,
    handle: AtspiHandle,
    mut events: AtspiEventReceiver,
    plane: Arc<DaemonAccessibilityPlane>,
    status: watch::Sender<AccessibilityRuntimeSnapshot>,
    cancellation: CancellationToken,
    maintenance_interval: Duration,
    operation_timeout: Duration,
    inter_page_delay: Duration,
    event_drain_limit: usize,
    event_publisher: AccessibilityEventPublisher,
) {
    if !enabled {
        publish_status(&status, &handle, &MirrorCursor::new());
        cancellation.cancelled().await;
        return;
    }

    let mut cursor = MirrorCursor::new();
    let mut maintenance = tokio::time::interval_at(
        tokio::time::Instant::now() + maintenance_interval,
        maintenance_interval,
    );
    maintenance.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            // Maintenance must outrank the continuously-ready event lane and
            // remain polled while the actor is transitional. Tokio evaluates a
            // select guard only when entering the select; guarding this timer
            // would otherwise make a later Healthy transition depend entirely
            // on a separate event notification to wake the mirror.
            _ = maintenance.tick() => {
                if mirror_needs_maintenance(&handle, &cursor) {
                    maintain_mirror(
                        &handle,
                        &plane,
                        &mut cursor,
                        &cancellation,
                        operation_timeout,
                        inter_page_delay,
                        &event_publisher,
                    ).await;
                    let drain = drain_pending_events(
                        &mut events,
                        &plane,
                        &mut cursor,
                        event_drain_limit,
                        &event_publisher,
                    ).await;
                    if drain == EventDrainOutcome::Empty {
                        settle_rebuild_barrier(&handle, &mut cursor);
                    }
                    if drain == EventDrainOutcome::Closed {
                        let generation = handle.health().accessibility_generation;
                        fence_until_rebuild(
                            &plane,
                            &mut cursor,
                            generation,
                            AccessibilityResyncReason::EventGap,
                            &event_publisher,
                        ).await;
                        cursor.rebuild_required = false;
                        publish_status(&status, &handle, &cursor);
                        break;
                    }
                }
                publish_status(&status, &handle, &cursor);
            }
            event = events.recv() => {
                let Some(event) = event else {
                    let generation = handle.health().accessibility_generation;
                    fence_until_rebuild(
                        &plane,
                        &mut cursor,
                        generation,
                        AccessibilityResyncReason::EventGap,
                        &event_publisher,
                    ).await;
                    cursor.rebuild_required = false;
                    publish_status(&status, &handle, &cursor);
                    break;
                };
                let overflow_epoch = events.overflow_epoch();
                process_event(
                    event,
                    &plane,
                    &mut cursor,
                    overflow_epoch,
                    &event_publisher,
                ).await;
                publish_status(&status, &handle, &cursor);
            }
        }
    }
}

async fn maintain_mirror(
    handle: &AtspiHandle,
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    cancellation: &CancellationToken,
    operation_timeout: Duration,
    inter_page_delay: Duration,
    event_publisher: &AccessibilityEventPublisher,
) {
    let health = handle.health();
    match health.state {
        AtspiActorState::Healthy => {
            if cursor.rebuild_required {
                if request_rebuild(handle, cancellation, operation_timeout).await {
                    cursor.awaiting_generation_after_rebuild =
                        Some(health.accessibility_generation);
                    cursor.covered_rebuild_overflow = None;
                    cursor.rebuild_required = false;
                }
                return;
            }
            if cursor
                .awaiting_generation_after_rebuild
                .is_some_and(|generation| health.accessibility_generation <= generation)
            {
                return;
            }
            if cursor.ready_for(health.accessibility_generation) {
                return;
            }
            if prepare_generation(
                plane,
                cursor,
                health.accessibility_generation,
                AccessibilityResyncReason::GenerationChanged,
                event_publisher,
            )
            .await
            .is_err()
            {
                cursor.invalidate(true);
                return;
            }
            match bootstrap_mirror(
                handle,
                plane,
                health.accessibility_generation,
                health.cache_revision,
                cancellation,
                operation_timeout,
                inter_page_delay,
            )
            .await
            {
                Ok(event_overflow_epoch) => {
                    cursor.mirrored_generation = Some(health.accessibility_generation);
                    cursor.mirrored_revision = Some(health.cache_revision);
                    if cursor
                        .awaiting_generation_after_rebuild
                        .is_some_and(|previous| health.accessibility_generation > previous)
                    {
                        cursor.covered_rebuild_overflow =
                            Some((health.accessibility_generation, event_overflow_epoch));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "AT-SPI mirror bootstrap raced or failed; rebuilding actor");
                    cursor.invalidate(true);
                }
            }
        }
        AtspiActorState::Reconnecting => {
            cursor.invalidate(false);
            let _ = prepare_generation(
                plane,
                cursor,
                health.accessibility_generation,
                AccessibilityResyncReason::GenerationChanged,
                event_publisher,
            )
            .await;
        }
        AtspiActorState::Connecting
        | AtspiActorState::Disabled
        | AtspiActorState::Stopped
        | AtspiActorState::Panicked => cursor.invalidate(false),
    }
}

async fn prepare_generation(
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    generation: u64,
    public_reason: AccessibilityResyncReason,
    event_publisher: &AccessibilityEventPublisher,
) -> Result<(), AccessibilityPlaneError> {
    if generation == cursor.prepared_generation {
        return Ok(());
    }
    let event = plane
        .begin_resync(generation, AccessibilityResyncCause::HealthGenerationChange)
        .await?;
    if event.kind != AccessibilityIngestKind::ResyncRequired {
        return Err(AccessibilityPlaneError::Internal);
    }
    event_publisher.publish_resync(&event, public_reason);
    cursor.prepared_generation = generation;
    Ok(())
}

async fn bootstrap_mirror(
    handle: &AtspiHandle,
    plane: &DaemonAccessibilityPlane,
    generation: u64,
    revision: u64,
    cancellation: &CancellationToken,
    operation_timeout: Duration,
    inter_page_delay: Duration,
) -> Result<u64, AccessibilityRuntimeMirrorError> {
    let mut after: Option<ObjectAddress> = None;
    loop {
        let page = timeout(
            operation_timeout,
            handle.cache_page(
                Some(generation),
                Some(revision),
                after.clone(),
                cancellation.child_token(),
            ),
        )
        .await
        .map_err(|_| AccessibilityRuntimeMirrorError::Timeout)??;
        if page.accessibility_generation != generation
            || page.revision != revision
            || page.after != after
        {
            return Err(AccessibilityRuntimeMirrorError::PageFence);
        }
        let next = page.next_after.clone();
        let event_overflow_epoch = page.event_overflow_epoch;
        let event = plane.ingest_cache_page(page).await?;
        if next.is_none() {
            if event.kind != AccessibilityIngestKind::Rebuilt {
                return Err(AccessibilityRuntimeMirrorError::PageFence);
            }
            return Ok(event_overflow_epoch);
        }
        if event.kind != AccessibilityIngestKind::BootstrapPending {
            return Err(AccessibilityRuntimeMirrorError::PageFence);
        }
        after = next;
        if inter_page_delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(inter_page_delay).await;
        }
    }
}

async fn request_rebuild(
    handle: &AtspiHandle,
    cancellation: &CancellationToken,
    operation_timeout: Duration,
) -> bool {
    match timeout(
        operation_timeout,
        handle.rebuild(cancellation.child_token()),
    )
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::warn!(%error, "AT-SPI actor rebuild request was not admitted");
            false
        }
        Err(_) => {
            tracing::warn!("AT-SPI actor rebuild request timed out");
            false
        }
    }
}

async fn process_event(
    event: AtspiActorEvent,
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    overflow_epoch: u64,
    event_publisher: &AccessibilityEventPublisher,
) {
    match event {
        AtspiActorEvent::HealthChanged(health) => {
            if health.state == AtspiActorState::Reconnecting {
                cursor.invalidate(false);
                let _ = prepare_generation(
                    plane,
                    cursor,
                    health.accessibility_generation,
                    AccessibilityResyncReason::GenerationChanged,
                    event_publisher,
                )
                .await;
            } else if matches!(
                health.state,
                AtspiActorState::Stopped | AtspiActorState::Panicked
            ) {
                fence_until_rebuild(
                    plane,
                    cursor,
                    health.accessibility_generation,
                    AccessibilityResyncReason::EventGap,
                    event_publisher,
                )
                .await;
                cursor.rebuild_required = false;
            } else if health.state != AtspiActorState::Healthy
                || cursor.mirrored_generation != Some(health.accessibility_generation)
            {
                cursor.invalidate(false);
            }
        }
        AtspiActorEvent::CacheChanged {
            accessibility_generation,
            previous_revision,
            revision,
            mutation,
            cached_nodes,
            cached_bytes,
        } => {
            tracing::trace!(
                cached_nodes,
                cached_bytes,
                "received bounded AT-SPI cache mutation"
            );
            if event_is_covered(cursor, accessibility_generation, revision) {
                return;
            }
            if cursor.mirrored_generation != Some(accessibility_generation)
                || cursor.mirrored_revision != Some(previous_revision)
                || previous_revision.checked_add(1) != Some(revision)
            {
                fence_until_rebuild(
                    plane,
                    cursor,
                    accessibility_generation,
                    AccessibilityResyncReason::EventGap,
                    event_publisher,
                )
                .await;
                return;
            }
            let kind = mutation_kind(&mutation);
            let result = plane
                .ingest_mutation(
                    accessibility_generation,
                    previous_revision,
                    CacheMutation {
                        revision,
                        kind,
                        detail: mutation,
                    },
                )
                .await;
            match result {
                Ok(transition) => {
                    cursor.mirrored_revision = Some(revision);
                    event_publisher.publish_cache_transition(&transition);
                }
                Err(error) => {
                    tracing::warn!(?error, "AT-SPI incremental mirror rejected actor evidence");
                    fence_until_rebuild(
                        plane,
                        cursor,
                        accessibility_generation,
                        AccessibilityResyncReason::EventGap,
                        event_publisher,
                    )
                    .await;
                }
            }
        }
        AtspiActorEvent::ApplicationInvalidated {
            accessibility_generation,
            cache_revision,
            bus_name,
            application_generation,
        } => {
            if event_is_covered(cursor, accessibility_generation, cache_revision) {
                return;
            }
            match plane
                .ingest_application_invalidation(
                    accessibility_generation,
                    cache_revision,
                    bus_name,
                    application_generation,
                )
                .await
            {
                // The actor normally emits CacheChanged(ApplicationInvalidated)
                // immediately before this owner-lifetime marker. That first
                // event commits and publishes every exact removal, after which
                // event_is_covered returns above. If a future producer delivers
                // the marker without the paired cache event, publish only when
                // the plane actually committed a non-Unchanged removal.
                Ok(transition) => event_publisher.publish_cache_transition(&transition),
                Err(_) => {
                    fence_until_rebuild(
                        plane,
                        cursor,
                        accessibility_generation,
                        AccessibilityResyncReason::EventGap,
                        event_publisher,
                    )
                    .await;
                }
            }
        }
        AtspiActorEvent::ResyncRequired {
            accessibility_generation,
            cache_revision,
            reason,
        } => {
            tracing::warn!(
                reason,
                accessibility_generation,
                cache_revision,
                prepared_generation = cursor.prepared_generation,
                mirrored_generation = ?cursor.mirrored_generation,
                awaiting_generation_after_rebuild = ?cursor.awaiting_generation_after_rebuild,
                covered_rebuild_overflow = ?cursor.covered_rebuild_overflow,
                overflow_epoch,
                rebuild_required = cursor.rebuild_required,
                "AT-SPI actor requested mirror resynchronization"
            );
            let completes_requested_rebuild = cursor
                .awaiting_generation_after_rebuild
                .is_some_and(|previous| accessibility_generation > previous);
            if completes_requested_rebuild {
                // Consume, rather than revision-skip, the actor's generation
                // barrier. The replacement snapshot is fenced to this newer
                // generation and a drain-to-empty must still complete before
                // the mirror can become externally ready.
                let _ = prepare_generation(
                    plane,
                    cursor,
                    accessibility_generation,
                    actor_resync_reason(reason),
                    event_publisher,
                )
                .await;
            } else if reason == "public_event_queue_overflow"
                && rebuild_overflow_is_covered(cursor, accessibility_generation, overflow_epoch)
            {
                tracing::debug!(
                    accessibility_generation,
                    overflow_epoch,
                    "consumed overflow covered by the completed actor rebuild fence"
                );
            // Receiver overflow is the only marker that is not paired with an
            // actor-owned cache invalidation. Every other reason is emitted as
            // part of a generation change, so requesting another rebuild when
            // that delayed marker arrives would create a reconnect loop.
            } else if reason == "public_event_queue_overflow"
                && public_overflow_requires_rebuild(cursor, accessibility_generation)
            {
                fence_until_rebuild(
                    plane,
                    cursor,
                    accessibility_generation,
                    AccessibilityResyncReason::EventQueueOverflow,
                    event_publisher,
                )
                .await;
            } else {
                cursor.invalidate(false);
                let _ = prepare_generation(
                    plane,
                    cursor,
                    accessibility_generation,
                    actor_resync_reason(reason),
                    event_publisher,
                )
                .await;
            }
        }
        AtspiActorEvent::ObjectChanged {
            accessibility_generation,
            cache_revision,
            source,
            kind,
        } => {
            tracing::trace!(
                source_known = source.is_some(),
                kind,
                "AT-SPI object metadata event"
            );
            match object_event_disposition(cursor, accessibility_generation, cache_revision) {
                ObjectEventDisposition::Suppress => {
                    tracing::trace!(
                        accessibility_generation,
                        cache_revision,
                        mirrored_generation = ?cursor.mirrored_generation,
                        mirrored_revision = ?cursor.mirrored_revision,
                        "suppressed AT-SPI object event outside the ready mirror fence"
                    );
                    return;
                }
                ObjectEventDisposition::Fence { current_generation } => {
                    tracing::warn!(
                        accessibility_generation,
                        cache_revision,
                        mirrored_generation = ?cursor.mirrored_generation,
                        mirrored_revision = ?cursor.mirrored_revision,
                        "AT-SPI object event advanced beyond the ready mirror fence"
                    );
                    fence_until_rebuild(
                        plane,
                        cursor,
                        current_generation,
                        AccessibilityResyncReason::EventGap,
                        event_publisher,
                    )
                    .await;
                    return;
                }
                ObjectEventDisposition::Resolve => {}
            }
            let Some(source) = source else {
                event_publisher.require_global_resync();
                return;
            };
            match plane
                .resolve_event_source(accessibility_generation, cache_revision, source)
                .await
            {
                Ok(source) => event_publisher.publish_object(&source, &kind),
                Err(error) => {
                    tracing::warn!(?error, "AT-SPI object source resolution failed closed");
                    event_publisher.require_global_resync();
                }
            }
        }
        AtspiActorEvent::ApplicationDegraded { bus_name, reason } => {
            tracing::warn!(
                bus_name,
                reason,
                "AT-SPI application requires bounded fallback"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectEventDisposition {
    Suppress,
    Resolve,
    Fence { current_generation: u64 },
}

fn object_event_disposition(
    cursor: &MirrorCursor,
    accessibility_generation: u64,
    cache_revision: u64,
) -> ObjectEventDisposition {
    if cursor.rebuild_required || cursor.awaiting_generation_after_rebuild.is_some() {
        return ObjectEventDisposition::Suppress;
    }
    let (Some(mirrored_generation), Some(mirrored_revision)) =
        (cursor.mirrored_generation, cursor.mirrored_revision)
    else {
        return ObjectEventDisposition::Suppress;
    };
    if accessibility_generation < mirrored_generation
        || (accessibility_generation == mirrored_generation && cache_revision < mirrored_revision)
    {
        return ObjectEventDisposition::Suppress;
    }
    if accessibility_generation > mirrored_generation || cache_revision > mirrored_revision {
        return ObjectEventDisposition::Fence {
            current_generation: accessibility_generation,
        };
    }
    ObjectEventDisposition::Resolve
}

#[cfg(test)]
pub(crate) fn object_event_fence_generation_for_test(
    cursor: &MirrorCursor,
    accessibility_generation: u64,
    cache_revision: u64,
) -> Option<u64> {
    match object_event_disposition(cursor, accessibility_generation, cache_revision) {
        ObjectEventDisposition::Fence { current_generation } => Some(current_generation),
        ObjectEventDisposition::Suppress | ObjectEventDisposition::Resolve => None,
    }
}

#[cfg(test)]
pub(crate) async fn process_event_for_test(
    event: AtspiActorEvent,
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    overflow_epoch: u64,
    event_publisher: &AccessibilityEventPublisher,
) {
    process_event(event, plane, cursor, overflow_epoch, event_publisher).await;
}

async fn fence_until_rebuild(
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    current_generation: u64,
    public_reason: AccessibilityResyncReason,
    event_publisher: &AccessibilityEventPublisher,
) {
    cursor.invalidate(true);
    let Some(next_generation) = current_generation.checked_add(1) else {
        return;
    };
    let cause = if public_reason == AccessibilityResyncReason::EventQueueOverflow {
        AccessibilityResyncCause::EventQueueOverflow
    } else {
        AccessibilityResyncCause::EventGap
    };
    match plane.begin_resync(next_generation, cause).await {
        Ok(event) => {
            event_publisher.publish_resync(&event, public_reason);
            cursor.prepared_generation = next_generation;
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                current_generation,
                "accessibility mirror could not install an immediate resync fence"
            );
        }
    }
}

fn actor_resync_reason(reason: &str) -> AccessibilityResyncReason {
    if reason == "public_event_queue_overflow" {
        AccessibilityResyncReason::EventQueueOverflow
    } else {
        AccessibilityResyncReason::ActorSignal
    }
}

const fn mutation_kind(detail: &CacheMutationDetail) -> CacheMutationKind {
    match detail {
        CacheMutationDetail::Upserted(_) => CacheMutationKind::Upserted,
        CacheMutationDetail::Refreshed(_) => CacheMutationKind::Refreshed,
        CacheMutationDetail::Removed(_) => CacheMutationKind::Removed,
        CacheMutationDetail::ApplicationInvalidated { .. } => {
            CacheMutationKind::ApplicationInvalidated
        }
        CacheMutationDetail::Unchanged => CacheMutationKind::Unchanged,
        CacheMutationDetail::ResyncRequired => CacheMutationKind::ResyncRequired,
    }
}

fn event_is_covered(cursor: &MirrorCursor, generation: u64, revision: u64) -> bool {
    cursor.mirrored_generation == Some(generation)
        && cursor
            .mirrored_revision
            .is_some_and(|mirrored| revision <= mirrored)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventDrainOutcome {
    Empty,
    Closed,
    BudgetExhausted,
}

async fn drain_pending_events(
    events: &mut AtspiEventReceiver,
    plane: &DaemonAccessibilityPlane,
    cursor: &mut MirrorCursor,
    limit: usize,
    event_publisher: &AccessibilityEventPublisher,
) -> EventDrainOutcome {
    for _ in 0..=limit {
        match events.try_recv() {
            AtspiTryRecv::Event(event) => {
                let overflow_epoch = events.overflow_epoch();
                process_event(event, plane, cursor, overflow_epoch, event_publisher).await;
            }
            AtspiTryRecv::Empty => return EventDrainOutcome::Empty,
            AtspiTryRecv::Closed => return EventDrainOutcome::Closed,
        }
    }
    EventDrainOutcome::BudgetExhausted
}

fn rebuild_overflow_is_covered(
    cursor: &MirrorCursor,
    generation: u64,
    overflow_epoch: u64,
) -> bool {
    cursor
        .covered_rebuild_overflow
        .is_some_and(|(covered_generation, covered_epoch)| {
            generation == covered_generation && overflow_epoch <= covered_epoch
        })
}

fn settle_rebuild_barrier(handle: &AtspiHandle, cursor: &mut MirrorCursor) {
    let health = handle.health();
    if cursor
        .awaiting_generation_after_rebuild
        .is_some_and(|previous| health.accessibility_generation > previous)
        && cursor.ready_for(health.accessibility_generation)
    {
        cursor.awaiting_generation_after_rebuild = None;
    }
}

fn public_overflow_requires_rebuild(cursor: &MirrorCursor, generation: u64) -> bool {
    cursor.mirrored_generation == Some(generation)
        || (cursor.mirrored_generation.is_none()
            && cursor.prepared_generation == 1
            && cursor.awaiting_generation_after_rebuild.is_none()
            && !cursor.rebuild_required)
}

fn publish_status(
    status: &watch::Sender<AccessibilityRuntimeSnapshot>,
    handle: &AtspiHandle,
    cursor: &MirrorCursor,
) {
    let health = handle.health();
    let mirror_ready = health.state == AtspiActorState::Healthy
        && cursor.mirrored_generation == Some(health.accessibility_generation)
        && cursor.mirrored_revision == Some(health.cache_revision)
        && cursor.awaiting_generation_after_rebuild.is_none();
    let next = AccessibilityRuntimeSnapshot {
        actor_state: health.state,
        mirror_ready,
        accessibility_generation: health.accessibility_generation,
        cache_revision: health.cache_revision,
    };
    let previous = *status.borrow();
    let lifecycle_changed = previous.actor_state != next.actor_state
        || previous.mirror_ready != next.mirror_ready
        || previous.accessibility_generation != next.accessibility_generation;
    let changed = status.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
    // Cache revision changes are intentionally frequent during event bursts.
    // Continue publishing every revision to readers, but only log lifecycle
    // transitions so debug logging remains bounded by meaningful state changes.
    if changed && lifecycle_changed {
        tracing::debug!(
            actor_state = ?next.actor_state,
            mirror_ready = next.mirror_ready,
            accessibility_generation = next.accessibility_generation,
            cache_revision = next.cache_revision,
            "accessibility runtime status changed"
        );
    }
}

fn mirror_needs_maintenance(handle: &AtspiHandle, cursor: &MirrorCursor) -> bool {
    let health = handle.health();
    health.state == AtspiActorState::Healthy
        && (!cursor.ready_for(health.accessibility_generation)
            || cursor.rebuild_required
            || cursor.awaiting_generation_after_rebuild.is_some())
}

#[derive(Debug, Error)]
pub(crate) enum AccessibilityRuntimeError {
    #[error("validated accessibility configuration could not be mapped safely")]
    InvalidConfiguration,
    #[error(transparent)]
    Actor(#[from] AtspiActorSpawnError),
    #[error("accessibility plane rejected runtime composition evidence: {0:?}")]
    Plane(AccessibilityPlaneError),
}

#[derive(Debug, Error)]
enum AccessibilityRuntimeMirrorError {
    #[error("AT-SPI actor cache request timed out")]
    Timeout,
    #[error("AT-SPI actor cache page changed generation, revision, or continuation")]
    PageFence,
    #[error(transparent)]
    Actor(#[from] AtspiActorError),
    #[error("accessibility mirror rejected actor evidence: {0:?}")]
    Plane(AccessibilityPlaneError),
}

impl From<AccessibilityPlaneError> for AccessibilityRuntimeError {
    fn from(error: AccessibilityPlaneError) -> Self {
        Self::Plane(error)
    }
}

impl From<AccessibilityPlaneError> for AccessibilityRuntimeMirrorError {
    fn from(error: AccessibilityPlaneError) -> Self {
        Self::Plane(error)
    }
}
