//! Pure-seam actor lifecycle, reconnect, cancellation, and overflow tests.

use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use xenoteer_atspi::{
    AtspiActorConfig, AtspiActorError, AtspiActorEvent, AtspiActorExit, AtspiActorHealth,
    AtspiActorState, AtspiBackend, AtspiBackendConnector, BackendEvent, BackendEventIngress,
    BackendFailure, BackendFailureKind, BackendFuture, BackendObservationRequest,
    BackendRefreshRequest, CacheEvent, CacheLimits, CachedLiveMetadata, EventOfferResult,
    NormalizedCacheItem, ObjectAddress, RedactedText, RefreshedCacheItem, SemanticDispatchMarker,
    SemanticError, SemanticEvidence, SemanticObservationEvidence, SemanticObservationRequest,
    SemanticOperation, SemanticRect, SemanticRequest, SemanticTarget, SemanticTargetRequest,
    SemanticValueEvidence, TextInsertPosition, TextProtection, TextSelectionPolicy,
    TextVerificationMode, spawn_atspi_actor,
};

type BootstrapPlan = Result<Vec<NormalizedCacheItem>, BackendFailure>;
type BootstrapPlans = Arc<Mutex<VecDeque<BootstrapPlan>>>;

#[derive(Clone, Debug)]
struct FakeControl {
    plans: BootstrapPlans,
    ingresses: Arc<Mutex<Vec<BackendEventIngress>>>,
    connections: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    overflow_first_connection: bool,
    retry_degraded_first_connection: bool,
    semantic_calls: Arc<AtomicUsize>,
    stall_after_dispatch: bool,
    panic_after_dispatch: bool,
    wait_in_preflight: bool,
    preflight_started: Arc<AtomicUsize>,
    preflight_release: Arc<tokio::sync::Notify>,
    semantic_dispatches: Arc<AtomicUsize>,
    refresh_calls: Arc<AtomicUsize>,
    refresh_states: Option<Vec<u32>>,
    refresh_flood_first_connection: bool,
    observation_calls: Arc<AtomicUsize>,
    wait_in_observation: bool,
    observation_started: Arc<AtomicUsize>,
    observation_release: Arc<tokio::sync::Notify>,
    stall_observation: bool,
    panic_observation: bool,
}

impl FakeControl {
    fn new(plans: Vec<BootstrapPlan>) -> Self {
        Self {
            plans: Arc::new(Mutex::new(VecDeque::from(plans))),
            ingresses: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            overflow_first_connection: false,
            retry_degraded_first_connection: false,
            semantic_calls: Arc::new(AtomicUsize::new(0)),
            stall_after_dispatch: false,
            panic_after_dispatch: false,
            wait_in_preflight: false,
            preflight_started: Arc::new(AtomicUsize::new(0)),
            preflight_release: Arc::new(tokio::sync::Notify::new()),
            semantic_dispatches: Arc::new(AtomicUsize::new(0)),
            refresh_calls: Arc::new(AtomicUsize::new(0)),
            refresh_states: None,
            refresh_flood_first_connection: false,
            observation_calls: Arc::new(AtomicUsize::new(0)),
            wait_in_observation: false,
            observation_started: Arc::new(AtomicUsize::new(0)),
            observation_release: Arc::new(tokio::sync::Notify::new()),
            stall_observation: false,
            panic_observation: false,
        }
    }

    fn stall_after_dispatch(mut self) -> Self {
        self.stall_after_dispatch = true;
        self
    }

    fn panic_after_dispatch(mut self) -> Self {
        self.panic_after_dispatch = true;
        self
    }

    fn wait_in_preflight(mut self) -> Self {
        self.wait_in_preflight = true;
        self
    }

    fn overflow_first_connection(mut self) -> Self {
        self.overflow_first_connection = true;
        self
    }

    fn retry_degraded_first_connection(mut self) -> Self {
        self.retry_degraded_first_connection = true;
        self
    }

    fn refresh_states(mut self, states: Vec<u32>) -> Self {
        self.refresh_states = Some(states);
        self
    }

    fn refresh_flood_first_connection(mut self) -> Self {
        self.refresh_flood_first_connection = true;
        self
    }

    fn wait_in_observation(mut self) -> Self {
        self.wait_in_observation = true;
        self
    }

    fn stall_observation(mut self) -> Self {
        self.stall_observation = true;
        self
    }

    fn panic_observation(mut self) -> Self {
        self.panic_observation = true;
        self
    }

    fn latest_ingress(&self) -> Result<BackendEventIngress, Box<dyn Error>> {
        self.ingresses
            .lock()
            .map_err(|_| "fake ingress lock poisoned")?
            .last()
            .cloned()
            .ok_or_else(|| "actor has not connected".into())
    }
}

#[derive(Debug)]
struct FakeBackend {
    bootstrap: Option<Result<Vec<NormalizedCacheItem>, BackendFailure>>,
    shutdowns: Arc<AtomicUsize>,
    semantic_calls: Arc<AtomicUsize>,
    stall_after_dispatch: bool,
    panic_after_dispatch: bool,
    wait_in_preflight: bool,
    preflight_started: Arc<AtomicUsize>,
    preflight_release: Arc<tokio::sync::Notify>,
    semantic_dispatches: Arc<AtomicUsize>,
    refresh_calls: Arc<AtomicUsize>,
    refresh_item: Option<NormalizedCacheItem>,
    refresh_states: Option<Vec<u32>>,
    observation_calls: Arc<AtomicUsize>,
    wait_in_observation: bool,
    observation_started: Arc<AtomicUsize>,
    observation_release: Arc<tokio::sync::Notify>,
    stall_observation: bool,
    panic_observation: bool,
}

impl AtspiBackend for FakeBackend {
    fn bootstrap(
        &mut self,
        _limits: CacheLimits,
        _proxy_call_timeout: Duration,
    ) -> BackendFuture<'_, Result<Vec<NormalizedCacheItem>, BackendFailure>> {
        let result = self.bootstrap.take().unwrap_or_else(|| {
            Err(BackendFailure::new(
                BackendFailureKind::Protocol,
                "bootstrap called more than once",
            ))
        });
        Box::pin(async move { result })
    }

    fn shutdown(&mut self) -> BackendFuture<'_, ()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn execute_semantic(
        &mut self,
        request: xenoteer_atspi::BackendSemanticRequest,
        dispatch: SemanticDispatchMarker,
    ) -> BackendFuture<'_, Result<SemanticEvidence, BackendFailure>> {
        self.semantic_calls.fetch_add(1, Ordering::SeqCst);
        let stall = self.stall_after_dispatch;
        let panic_after_dispatch = self.panic_after_dispatch;
        let wait_in_preflight = self.wait_in_preflight;
        let preflight_started = Arc::clone(&self.preflight_started);
        let preflight_release = Arc::clone(&self.preflight_release);
        let semantic_dispatches = Arc::clone(&self.semantic_dispatches);
        Box::pin(async move {
            if wait_in_preflight {
                preflight_started.fetch_add(1, Ordering::SeqCst);
                preflight_release.notified().await;
            }
            request.dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            semantic_dispatches.fetch_add(1, Ordering::SeqCst);
            if panic_after_dispatch {
                std::panic::resume_unwind(Box::new("fake semantic panic after dispatch"));
            }
            if stall {
                std::future::pending().await
            } else {
                Ok(SemanticEvidence::Focus {
                    accepted: true,
                    focused: true,
                })
            }
        })
    }

    fn refresh_object(
        &mut self,
        request: BackendRefreshRequest,
    ) -> BackendFuture<'_, Result<RefreshedCacheItem, BackendFailure>> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        let mut item = self.refresh_item.clone();
        let states = self.refresh_states.clone();
        Box::pin(async move {
            let mut item = item.take().ok_or_else(|| {
                BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "fake refresh item was not configured",
                )
            })?;
            if item.object != request.object || item.application != request.expected_application {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "fake refresh provenance mismatch",
                ));
            }
            if let Some(states) = states {
                item.states = states;
            }
            Ok(RefreshedCacheItem {
                item,
                live: CachedLiveMetadata::default(),
            })
        })
    }

    fn observe_exact(
        &mut self,
        request: BackendObservationRequest,
    ) -> BackendFuture<'_, Result<SemanticObservationEvidence, BackendFailure>> {
        self.observation_calls.fetch_add(1, Ordering::SeqCst);
        let wait = self.wait_in_observation;
        let started = Arc::clone(&self.observation_started);
        let release = Arc::clone(&self.observation_release);
        let stall = self.stall_observation;
        let panic = self.panic_observation;
        Box::pin(async move {
            if wait {
                started.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
            }
            request.read_permit.ensure_current()?;
            if panic {
                std::panic::resume_unwind(Box::new("fake observation panic"));
            }
            if stall {
                std::future::pending::<()>().await;
            }
            Ok(SemanticObservationEvidence {
                identity_fingerprint: request.expected_identity,
                parent: None,
                index_in_parent: request.expected_index_in_parent,
                role: request.expected_role,
                states: vec![42],
                interfaces: vec!["org.a11y.atspi.Component".to_owned()],
                bounds: Some(SemanticRect {
                    x: 10,
                    y: 20,
                    width: 30,
                    height: 40,
                }),
                top_level: Some(request.object),
                application_pid: Some(4_242),
                value: Some(SemanticValueEvidence {
                    current: 5.0,
                    minimum: 0.0,
                    maximum: 10.0,
                    minimum_increment: 1.0,
                }),
                text: None,
                selected_children: Some(2),
            })
        })
    }
}

impl AtspiBackendConnector for FakeControl {
    type Backend = FakeBackend;

    fn connect(
        &mut self,
        ingress: BackendEventIngress,
        _cache_limits: CacheLimits,
    ) -> BackendFuture<'_, Result<Self::Backend, BackendFailure>> {
        let number = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut ingresses) = self.ingresses.lock() {
            ingresses.push(ingress.clone());
        }
        if self.overflow_first_connection && number == 1 {
            let _first = ingress.offer(BackendEvent::ObjectChanged {
                source: None,
                kind: "first".to_owned(),
            });
            let _overflow = ingress.offer(BackendEvent::ObjectChanged {
                source: None,
                kind: "second".to_owned(),
            });
        }
        if self.retry_degraded_first_connection && number == 1 {
            let retry_ingress = ingress.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let _result = retry_ingress.offer(BackendEvent::ResyncRequired {
                    reason: "degraded_application_retry",
                });
            });
        }
        let plan = self
            .plans
            .lock()
            .ok()
            .and_then(|mut plans| plans.pop_front())
            .unwrap_or_else(|| Ok(Vec::new()));
        let refresh_item = plan.as_ref().ok().and_then(|items| items.first()).cloned();
        if self.refresh_flood_first_connection
            && number == 1
            && let Some(item) = &refresh_item
        {
            let _ = ingress.offer(BackendEvent::RefreshObject {
                source: item.object.clone(),
                kind: "focus.changed".to_owned(),
            });
            let _ = ingress.offer(BackendEvent::RefreshObject {
                source: item.object.clone(),
                kind: "object.state_changed".to_owned(),
            });
        }
        let shutdowns = Arc::clone(&self.shutdowns);
        let semantic_calls = Arc::clone(&self.semantic_calls);
        let stall_after_dispatch = self.stall_after_dispatch;
        let panic_after_dispatch = self.panic_after_dispatch;
        let wait_in_preflight = self.wait_in_preflight;
        let preflight_started = Arc::clone(&self.preflight_started);
        let preflight_release = Arc::clone(&self.preflight_release);
        let semantic_dispatches = Arc::clone(&self.semantic_dispatches);
        let refresh_calls = Arc::clone(&self.refresh_calls);
        let refresh_states = self.refresh_states.clone();
        let observation_calls = Arc::clone(&self.observation_calls);
        let wait_in_observation = self.wait_in_observation;
        let observation_started = Arc::clone(&self.observation_started);
        let observation_release = Arc::clone(&self.observation_release);
        let stall_observation = self.stall_observation;
        let panic_observation = self.panic_observation;
        Box::pin(async move {
            Ok(FakeBackend {
                bootstrap: Some(plan),
                shutdowns,
                semantic_calls,
                stall_after_dispatch,
                panic_after_dispatch,
                wait_in_preflight,
                preflight_started,
                preflight_release,
                semantic_dispatches,
                refresh_calls,
                refresh_item,
                refresh_states,
                observation_calls,
                wait_in_observation,
                observation_started,
                observation_release,
                stall_observation,
                panic_observation,
            })
        })
    }
}

fn semantic_request(
    health: &AtspiActorHealth,
    node: &xenoteer_atspi::CachedNode,
    operation: SemanticOperation,
) -> SemanticRequest {
    SemanticRequest {
        target: semantic_target(health, node),
        operation,
        deadline: tokio::time::Instant::now() + Duration::from_secs(1),
    }
}

fn semantic_target(health: &AtspiActorHealth, node: &xenoteer_atspi::CachedNode) -> SemanticTarget {
    SemanticTarget {
        object: node.item.object.clone(),
        application: node.item.application.clone(),
        accessibility_generation: health.accessibility_generation,
        application_generation: node.application_generation,
        cache_revision: health.cache_revision,
        node_revision: node.revision,
        index_in_parent: node.item.index_in_parent,
        identity_fingerprint: node.identity_fingerprint.clone(),
        role: node.item.role,
        states: node.item.states.clone(),
    }
}

fn observation_request(
    health: &AtspiActorHealth,
    node: &xenoteer_atspi::CachedNode,
) -> SemanticObservationRequest {
    SemanticObservationRequest {
        target: semantic_target(health, node),
        deadline: tokio::time::Instant::now() + Duration::from_secs(1),
    }
}

fn semantic_target_request(
    health: &AtspiActorHealth,
    node: &xenoteer_atspi::CachedNode,
) -> SemanticTargetRequest {
    SemanticTargetRequest {
        object: node.item.object.clone(),
        application: node.item.application.clone(),
        accessibility_generation: health.accessibility_generation,
        application_generation: node.application_generation,
        cache_revision: health.cache_revision,
        node_revision: node.revision,
    }
}

fn node(bus: &str, suffix: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    Ok(NormalizedCacheItem {
        object: ObjectAddress::new(bus, format!("/test/{suffix}"))?,
        application: ObjectAddress::new(bus, "/test/app")?,
        parent: None,
        index_in_parent: None,
        child_count: Some(0),
        legacy_children: Vec::new(),
        interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
        name: suffix.to_owned(),
        description: String::new(),
        role: 0,
        text_protection: TextProtection::Unprotected,
        states: Vec::new(),
    })
}

fn test_config() -> AtspiActorConfig {
    AtspiActorConfig {
        backend_event_capacity: 1,
        event_capacity: 8,
        connect_timeout: Duration::from_millis(100),
        bootstrap_timeout: Duration::from_millis(100),
        shutdown_timeout: Duration::from_millis(100),
        reconnect_initial: Duration::from_millis(5),
        reconnect_max: Duration::from_millis(20),
        ..AtspiActorConfig::default()
    }
}

async fn wait_for_health(
    handle: &xenoteer_atspi::AtspiHandle,
    predicate: impl Fn(&AtspiActorHealth) -> bool,
) -> Result<AtspiActorHealth, Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let health = handle.health();
        if predicate(&health) {
            return Ok(health);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("health predicate deadline exceeded: {health:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn disabled_actor_never_connects_but_reports_health_and_shuts_down()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(Vec::new());
    let connections = Arc::clone(&control.connections);
    let spawned = spawn_atspi_actor(false, test_config(), control)?;
    let snapshot = spawned.handle.snapshot(CancellationToken::new()).await?;
    assert_eq!(snapshot.health.state, AtspiActorState::Disabled);
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn actor_bootstraps_cache_and_exposes_no_backend_proxy() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.10", "root")?])]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
    })
    .await?;
    assert_eq!(health.cached_nodes, 1);
    assert_eq!(health.accessibility_generation, 1);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn targeted_refresh_coalesces_and_orders_cache_before_object_events()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.103", "root")?])])
        .refresh_states(vec![5])
        .refresh_flood_first_connection();
    let refresh_calls = Arc::clone(&control.refresh_calls);
    let mut config = test_config();
    config.backend_event_capacity = 4;
    config.event_capacity = 16;
    let mut spawned = spawn_atspi_actor(true, config, control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cache_revision == 2
    })
    .await?;
    assert_eq!(health.accessibility_generation, 1);
    assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);

    let mut relevant = Vec::new();
    while relevant.len() < 3 {
        let event = tokio::time::timeout(Duration::from_secs(1), spawned.events.recv())
            .await?
            .ok_or("actor event stream closed")?;
        if matches!(
            event,
            AtspiActorEvent::CacheChanged { .. } | AtspiActorEvent::ObjectChanged { .. }
        ) {
            relevant.push(event);
        }
    }
    assert!(matches!(
        relevant.first(),
        Some(AtspiActorEvent::CacheChanged {
            revision: 2,
            mutation: xenoteer_atspi::CacheMutationDetail::Refreshed(_),
            ..
        })
    ));
    for event in &relevant[1..] {
        assert!(matches!(
            event,
            AtspiActorEvent::ObjectChanged {
                accessibility_generation: 1,
                cache_revision: 2,
                ..
            }
        ));
    }
    let page = spawned
        .handle
        .cache_page(Some(1), Some(2), None, CancellationToken::new())
        .await?;
    assert_eq!(page.nodes[0].item.states, vec![5]);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn explicit_reconcile_emits_change_once_and_preserves_revision_on_noop()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.104", "root")?])]).refresh_states(vec![9]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let initial = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(initial.accessibility_generation),
            Some(initial.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let request = semantic_target_request(&initial, &page.nodes[0]);
    let changed = spawned
        .handle
        .reconcile_semantic_target(
            request,
            tokio::time::Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await?;
    assert!(changed.changed);
    assert_eq!(changed.previous_cache_revision, 1);
    assert_eq!(changed.cache_revision, 2);
    assert_eq!(changed.accessibility_generation, 1);

    let current = spawned.handle.health();
    let page = spawned
        .handle
        .cache_page(Some(1), Some(2), None, CancellationToken::new())
        .await?;
    let unchanged = spawned
        .handle
        .reconcile_semantic_target(
            semantic_target_request(&current, &page.nodes[0]),
            tokio::time::Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await?;
    assert!(!unchanged.changed);
    assert_eq!(unchanged.previous_cache_revision, 2);
    assert_eq!(unchanged.cache_revision, 2);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn semantic_target_materializer_enforces_every_cache_coordinate() -> Result<(), Box<dyn Error>>
{
    let control = FakeControl::new(vec![Ok(vec![node(":1.111", "root")?])]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;
    let request = semantic_target_request(&health, &page.nodes[0]);
    let target = spawned
        .handle
        .semantic_target(request.clone(), CancellationToken::new())
        .await?;
    assert_eq!(
        target.identity_fingerprint,
        page.nodes[0].identity_fingerprint
    );

    let mut stale = request.clone();
    stale.accessibility_generation += 1;
    assert!(matches!(
        spawned
            .handle
            .semantic_target(stale, CancellationToken::new())
            .await,
        Err(SemanticError::StaleAccessibilityGeneration { .. })
    ));
    let mut stale = request.clone();
    stale.cache_revision += 1;
    assert!(matches!(
        spawned
            .handle
            .semantic_target(stale, CancellationToken::new())
            .await,
        Err(SemanticError::StaleCacheRevision { .. })
    ));
    let mut stale = request.clone();
    stale.application_generation += 1;
    assert!(matches!(
        spawned
            .handle
            .semantic_target(stale, CancellationToken::new())
            .await,
        Err(SemanticError::StaleApplicationGeneration { .. })
    ));
    let mut stale = request;
    stale.node_revision += 1;
    assert_eq!(
        spawned
            .handle
            .semantic_target(stale, CancellationToken::new())
            .await,
        Err(SemanticError::StaleIdentity)
    );
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn fresh_observation_has_monotonic_epochs_and_bounded_live_evidence()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.105", "root")?])]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let first = spawned
        .handle
        .observe_semantic(
            observation_request(&health, &page.nodes[0]),
            CancellationToken::new(),
        )
        .await?;
    let second = spawned
        .handle
        .observe_semantic(
            observation_request(&health, &page.nodes[0]),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!((first.read_epoch, second.read_epoch), (1, 2));
    assert_eq!(first.evidence.states, vec![42]);
    assert_eq!(first.evidence.bounds.map(|bounds| bounds.width), Some(30));
    assert_eq!(first.evidence.application_pid, Some(4_242));
    assert_eq!(first.evidence.selected_children, Some(2));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn fresh_observation_allows_unrelated_global_revision_advance() -> Result<(), Box<dyn Error>>
{
    let control = FakeControl::new(vec![Ok(vec![node(":1.105", "target")?])]);
    let observation = control.clone();
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let target = page.nodes.first().ok_or("missing observation target")?;
    let request = observation_request(&health, target);
    let target_request = semantic_target_request(&health, target);

    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(node(
                ":1.105",
                "unrelated",
            )?)))),
        EventOfferResult::Accepted
    );
    let advanced = wait_for_health(&spawned.handle, |current| {
        current.cache_revision > health.cache_revision
    })
    .await?;
    let reminted = spawned
        .handle
        .semantic_target(target_request.clone(), CancellationToken::new())
        .await?;
    assert_eq!(reminted.cache_revision, health.cache_revision);
    assert_eq!(reminted.node_revision, target.revision);
    let result = spawned
        .handle
        .observe_semantic(request, CancellationToken::new())
        .await?;
    assert_eq!(result.cache_revision, advanced.cache_revision);
    assert_eq!(result.object, target.item.object);

    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(node(
                ":1.105", "target",
            )?)))),
        EventOfferResult::Accepted
    );
    let _target_advanced = wait_for_health(&spawned.handle, |current| {
        current.cache_revision > advanced.cache_revision
    })
    .await?;
    assert_eq!(
        spawned
            .handle
            .semantic_target(target_request, CancellationToken::new())
            .await,
        Err(SemanticError::StaleIdentity)
    );
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn ingress_change_during_fresh_observation_rejects_the_read() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.106", "root")?])]).wait_in_observation();
    let observation = control.clone();
    let started = Arc::clone(&control.observation_started);
    let release = Arc::clone(&control.observation_release);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let handle = spawned.handle.clone();
    let request = observation_request(&health, &page.nodes[0]);
    let read = tokio::spawn(async move {
        handle
            .observe_semantic(request, CancellationToken::new())
            .await
    });
    wait_for_atomic(&started).await?;
    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::ObjectChanged {
                source: None,
                kind: "during-observation".to_owned(),
            }),
        EventOfferResult::Accepted
    );
    release.notify_waiters();
    assert!(matches!(read.await?, Err(SemanticError::Backend(_))));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_observation_seam_succeeds_without_a_runtime_block_on()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.107", "root")?])]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;
    let handle = spawned.handle.clone();
    let request = observation_request(&health, &page.nodes[0]);
    let result = tokio::task::spawn_blocking(move || {
        handle.observe_semantic_blocking(request, Duration::from_secs(1))
    })
    .await??;
    assert_eq!(result.read_epoch, 1);
    assert_eq!(result.evidence.application_pid, Some(4_242));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_observation_timeout_is_bounded() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.108", "root")?])]).stall_observation();
    let started = Arc::clone(&control.observation_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;
    let handle = spawned.handle.clone();
    let mut request = observation_request(&health, &page.nodes[0]);
    request.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let observation = tokio::task::spawn_blocking(move || {
        handle.observe_semantic_blocking(request, Duration::from_millis(20))
    });
    wait_for_atomic(&started).await?;
    let result = observation.await?;
    assert_eq!(result, Err(SemanticError::DeadlineBeforeDispatch));
    tokio::time::timeout(
        Duration::from_millis(100),
        spawned.handle.snapshot(CancellationToken::new()),
    )
    .await??;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_blocking_observation_at_queue_head_never_reaches_backend()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.111", "root")?])]).wait_in_observation();
    let started = Arc::clone(&control.observation_started);
    let calls = Arc::clone(&control.observation_calls);
    let release = Arc::clone(&control.observation_release);
    let mut config = test_config();
    config.request_capacity = 2;
    let spawned = spawn_atspi_actor(true, config, control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;

    let first_handle = spawned.handle.clone();
    let first_request = observation_request(&health, &page.nodes[0]);
    let first = tokio::spawn(async move {
        first_handle
            .observe_semantic(first_request, CancellationToken::new())
            .await
    });
    wait_for_atomic(&started).await?;

    let blocking_handle = spawned.handle.clone();
    let mut blocking_request = observation_request(&health, &page.nodes[0]);
    blocking_request.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let blocking = tokio::task::spawn_blocking(move || {
        blocking_handle.observe_semantic_blocking(blocking_request, Duration::from_millis(20))
    });
    assert_eq!(blocking.await?, Err(SemanticError::DeadlineBeforeDispatch));

    release.notify_waiters();
    assert!(first.await?.is_ok());
    tokio::time::timeout(
        Duration::from_millis(100),
        spawned.handle.snapshot(CancellationToken::new()),
    )
    .await??;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_observation_reply_loss_reports_stopped() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.109", "root")?])]).panic_observation();
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;
    let handle = spawned.handle.clone();
    let request = observation_request(&health, &page.nodes[0]);
    let result = tokio::task::spawn_blocking(move || {
        handle.observe_semantic_blocking(request, Duration::from_secs(1))
    })
    .await?;
    assert_eq!(result, Err(SemanticError::Stopped));
    assert_eq!(spawned.join.wait().await, AtspiActorExit::Panicked);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_observation_fails_fast_when_actor_queue_is_full() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.110", "root")?])]).wait_in_observation();
    let started = Arc::clone(&control.observation_started);
    let release = Arc::clone(&control.observation_release);
    let mut config = test_config();
    config.request_capacity = 1;
    let spawned = spawn_atspi_actor(true, config, control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(Some(1), Some(1), None, CancellationToken::new())
        .await?;
    let first_handle = spawned.handle.clone();
    let first_request = observation_request(&health, &page.nodes[0]);
    let first = tokio::spawn(async move {
        first_handle
            .observe_semantic(first_request, CancellationToken::new())
            .await
    });
    wait_for_atomic(&started).await?;

    let queued_cancellation = CancellationToken::new();
    let queued_handle = spawned.handle.clone();
    let queued_request = observation_request(&health, &page.nodes[0]);
    let queued_token = queued_cancellation.clone();
    let queued = tokio::spawn(async move {
        queued_handle
            .observe_semantic(queued_request, queued_token)
            .await
    });
    tokio::task::yield_now().await;
    let blocking_handle = spawned.handle.clone();
    let blocking_request = observation_request(&health, &page.nodes[0]);
    let result = tokio::task::spawn_blocking(move || {
        blocking_handle.observe_semantic_blocking(blocking_request, Duration::from_secs(1))
    })
    .await?;
    assert_eq!(result, Err(SemanticError::QueueFull));
    queued_cancellation.cancel();
    release.notify_waiters();
    assert!(first.await?.is_ok());
    assert_eq!(queued.await?, Err(SemanticError::CancelledBeforeDispatch));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn quiet_rebuild_has_no_public_overflow_after_the_final_page_fence()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![
        Ok(vec![node(":1.101", "first")?]),
        Ok(vec![node(":1.102", "second")?]),
    ]);
    let mut config = test_config();
    config.event_capacity = 1;
    let mut spawned = spawn_atspi_actor(true, config, control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
    })
    .await?;
    spawned.handle.rebuild(CancellationToken::new()).await?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.accessibility_generation == 2
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let fenced_epoch = page.event_overflow_epoch;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(spawned.events.overflow_epoch(), fenced_epoch);
    while !matches!(
        spawned.events.try_recv(),
        xenoteer_atspi::AtspiTryRecv::Empty
    ) {}
    assert_eq!(spawned.events.overflow_epoch(), fenced_epoch);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn connection_loss_invalidates_generation_and_rebuilds() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![
        Ok(vec![node(":1.11", "old")?]),
        Ok(vec![node(":1.12", "new")?]),
    ]);
    let observation = control.clone();
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::ConnectionClosed),
        EventOfferResult::Accepted
    );
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.accessibility_generation >= 2
    })
    .await?;
    assert_eq!(health.accessibility_generation, 2);
    assert_eq!(health.cached_nodes, 1);
    assert!(observation.connections.load(Ordering::SeqCst) >= 2);
    assert!(observation.shutdowns.load(Ordering::SeqCst) >= 1);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn degraded_application_retry_rebuilds_to_a_populated_subtree() -> Result<(), Box<dyn Error>>
{
    let degraded_root = node(":1.14", "degraded-root")?;
    let mut recovered_root = node(":1.14", "recovered-root")?;
    let mut recovered_child = node(":1.14", "recovered-child")?;
    recovered_root.child_count = Some(1);
    recovered_child.parent = Some(recovered_root.object.clone());
    recovered_child.index_in_parent = Some(0);
    let control = FakeControl::new(vec![
        Ok(vec![degraded_root]),
        Ok(vec![recovered_root, recovered_child]),
    ])
    .retry_degraded_first_connection();
    let observation = control.clone();
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
            && health.accessibility_generation >= 2
            && health.cached_nodes == 2
    })
    .await?;
    assert_eq!(health.accessibility_generation, 2);
    assert!(observation.connections.load(Ordering::SeqCst) >= 2);
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(page.nodes.len(), 2);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.item.name == "recovered-child")
    );
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn backend_ingress_overflow_forces_resync_and_reconnect() -> Result<(), Box<dyn Error>> {
    let control =
        FakeControl::new(vec![Ok(Vec::new()), Ok(Vec::new())]).overflow_first_connection();
    let observation = control.clone();
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.accessibility_generation >= 2
    })
    .await?;
    assert_eq!(health.accessibility_generation, 2);
    assert!(observation.connections.load(Ordering::SeqCst) >= 2);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn public_event_overflow_is_a_capacity_independent_resync() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(Vec::new())]);
    let observation = control.clone();
    let mut config = test_config();
    config.backend_event_capacity = 64;
    config.event_capacity = 1;
    let mut spawned = spawn_atspi_actor(true, config, control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
    })
    .await?;
    let ingress = observation.latest_ingress()?;
    for index in 0..32 {
        let _result = ingress.offer(BackendEvent::ObjectChanged {
            source: None,
            kind: format!("event-{index}"),
        });
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let event = spawned
        .events
        .recv()
        .await
        .ok_or("public event receiver closed")?;
    let barrier_revision = match event {
        AtspiActorEvent::ResyncRequired {
            reason: "public_event_queue_overflow",
            cache_revision,
            ..
        } => cache_revision,
        other => return Err(format!("expected overflow barrier, got {other:?}").into()),
    };
    assert_eq!(
        ingress.offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(node(
            ":1.18",
            "fresh-after-resync",
        )?)))),
        EventOfferResult::Accepted
    );
    let fresh = tokio::time::timeout(Duration::from_millis(100), spawned.events.recv())
        .await?
        .ok_or("public event receiver closed after resync")?;
    match fresh {
        AtspiActorEvent::CacheChanged {
            previous_revision,
            revision,
            mutation: xenoteer_atspi::CacheMutationDetail::Upserted(node),
            ..
        } => {
            assert_eq!(previous_revision, barrier_revision);
            assert_eq!(revision, barrier_revision + 1);
            assert_eq!(node.item.name, "fresh-after-resync");
        }
        other => return Err(format!("expected fresh fenced mutation, got {other:?}").into()),
    }
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn caller_cancellation_wins_request_result() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(Vec::new())]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        spawned.handle.snapshot(cancellation).await,
        Err(AtspiActorError::Cancelled)
    );
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn dropping_all_handles_stops_instead_of_spinning_on_a_closed_queue()
-> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(Vec::new())]);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
    })
    .await?;
    drop(spawned.handle);
    drop(spawned.events);
    let exit = tokio::time::timeout(Duration::from_millis(100), spawned.join.wait()).await?;
    assert_eq!(exit, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn cache_pages_are_bounded_and_revision_fenced() -> Result<(), Box<dyn Error>> {
    let control = FakeControl::new(vec![Ok(vec![node(":1.20", "one")?, node(":1.20", "two")?])]);
    let observation = control.clone();
    let mut config = test_config();
    config.read_page_nodes = 1;
    let spawned = spawn_atspi_actor(true, config, control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 2
    })
    .await?;
    let first = spawned
        .handle
        .cache_page(None, None, None, CancellationToken::new())
        .await?;
    assert_eq!(first.nodes.len(), 1);
    let after = first.next_after.clone().ok_or("first page lacked cursor")?;
    assert_eq!(
        spawned
            .handle
            .cache_page(
                None,
                Some(first.revision),
                Some(after.clone()),
                CancellationToken::new(),
            )
            .await,
        Err(AtspiActorError::InvalidPage)
    );
    let second = spawned
        .handle
        .cache_page(
            Some(first.accessibility_generation),
            Some(first.revision),
            Some(after),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(second.nodes.len(), 1);
    assert!(second.next_after.is_none());

    let third = node(":1.20", "three")?;
    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(third)))),
        EventOfferResult::Accepted
    );
    wait_for_health(&spawned.handle, |health| {
        health.cache_revision > first.revision
    })
    .await?;
    assert!(matches!(
        spawned
            .handle
            .cache_page(
                Some(first.accessibility_generation),
                Some(first.revision),
                first.next_after,
                CancellationToken::new(),
            )
            .await,
        Err(AtspiActorError::StaleRevision { .. })
    ));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[test]
fn secret_text_debug_is_redacted_and_action_names_reject_nul() -> Result<(), Box<dyn Error>> {
    let secret = RedactedText::new("do-not-log-this")?;
    let debug = format!("{secret:?}");
    assert!(!debug.contains("do-not-log-this"));
    assert!(debug.contains("utf8_bytes"));
    let operation = SemanticOperation::Invoke(xenoteer_atspi::ActionSelector::Name(
        "click\0hidden".to_owned(),
    ));
    assert!(matches!(
        operation.validate(),
        Err(SemanticError::InvalidRequest(_))
    ));
    Ok(())
}

#[tokio::test]
async fn semantic_actor_revalidates_exact_identity_before_backend_dispatch()
-> Result<(), Box<dyn Error>> {
    let mut item = node(":1.30", "focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]);
    let calls = Arc::clone(&control.semantic_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let cached = page.nodes.first().ok_or("missing cached semantic node")?;

    let result = spawned
        .handle
        .execute_semantic(
            semantic_request(&health, cached, SemanticOperation::Focus),
            CancellationToken::new(),
        )
        .await?;
    assert!(matches!(
        result.evidence,
        SemanticEvidence::Focus {
            accepted: true,
            focused: true
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut stale = semantic_request(&health, cached, SemanticOperation::Focus);
    stale.target.node_revision = stale.target.node_revision.saturating_add(1);
    assert_eq!(
        spawned
            .handle
            .execute_semantic(stale, CancellationToken::new())
            .await,
        Err(SemanticError::StaleIdentity)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn protected_text_write_is_length_only_and_secret_safe() -> Result<(), Box<dyn Error>> {
    let mut item = node(":1.31", "password")?;
    item.interfaces.extend([
        "org.a11y.atspi.EditableText".to_owned(),
        "org.a11y.atspi.Text".to_owned(),
    ]);
    item.role = 40;
    item.text_protection = TextProtection::Protected;
    let control = FakeControl::new(vec![Ok(vec![item])]);
    let calls = Arc::clone(&control.semantic_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let cached = page.nodes.first().ok_or("missing password node")?;
    let denied_exact = semantic_request(
        &health,
        cached,
        SemanticOperation::SetText {
            text: RedactedText::new("protected-exact-secret-value")?,
            selection: TextSelectionPolicy::CollapseAfter,
            verification: TextVerificationMode::Exact,
        },
    );
    assert!(
        !format!("{denied_exact:?}").contains("protected-exact-secret-value"),
        "exact protected request Debug must remain redacted"
    );
    let denied_result = spawned
        .handle
        .execute_semantic(denied_exact, CancellationToken::new())
        .await;
    assert!(matches!(
        denied_result,
        Err(SemanticError::InvalidRequest(
            "exact verification is denied for protected text"
        ))
    ));
    assert!(
        !format!("{denied_result:?}").contains("protected-exact-secret-value"),
        "exact protected failure Debug must remain redacted"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let request = semantic_request(
        &health,
        cached,
        SemanticOperation::SetText {
            text: RedactedText::new("protected-secret-value")?,
            selection: TextSelectionPolicy::CollapseAfter,
            verification: TextVerificationMode::LengthOnly,
        },
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains("protected-secret-value"));
    spawned
        .handle
        .execute_semantic(request, CancellationToken::new())
        .await?;
    let insert = semantic_request(
        &health,
        cached,
        SemanticOperation::InsertText {
            position: TextInsertPosition::LiveCaret,
            text: RedactedText::new("protected-insert-secret")?,
            selection: TextSelectionPolicy::CollapseAfter,
            verification: TextVerificationMode::LengthOnly,
        },
    );
    assert!(!format!("{insert:?}").contains("protected-insert-secret"));
    let result = spawned
        .handle
        .execute_semantic(insert, CancellationToken::new())
        .await;
    assert!(!format!("{result:?}").contains("protected-insert-secret"));
    result?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn unknown_role_text_write_fails_closed_without_backend_dispatch()
-> Result<(), Box<dyn Error>> {
    let mut item = node(":1.32", "future-secret")?;
    item.interfaces.extend([
        "org.a11y.atspi.EditableText".to_owned(),
        "org.a11y.atspi.Text".to_owned(),
    ]);
    item.role = u32::MAX;
    item.text_protection = TextProtection::Unknown;
    let control = FakeControl::new(vec![Ok(vec![item])]);
    let calls = Arc::clone(&control.semantic_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let cached = page.nodes.first().ok_or("missing unknown-role node")?;
    let request = semantic_request(
        &health,
        cached,
        SemanticOperation::SetText {
            text: RedactedText::new("unknown-secret-value")?,
            selection: TextSelectionPolicy::CollapseAfter,
            verification: TextVerificationMode::LengthOnly,
        },
    );
    let result = spawned
        .handle
        .execute_semantic(request, CancellationToken::new())
        .await;
    assert_eq!(result, Err(SemanticError::UnclassifiedTextDenied));
    assert!(!format!("{result:?}").contains("unknown-secret-value"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn queued_removal_is_reconciled_before_semantic_dispatch() -> Result<(), Box<dyn Error>> {
    let mut item = node(":1.33", "removed-before-focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]);
    let observation = control.clone();
    let calls = Arc::clone(&control.semantic_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let cached = page.nodes.first().ok_or("missing semantic node")?;
    let request = semantic_request(&health, cached, SemanticOperation::Focus);
    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::Remove(
                cached.item.object.clone()
            ))),
        EventOfferResult::Accepted
    );
    assert!(matches!(
        spawned
            .handle
            .execute_semantic(request, CancellationToken::new())
            .await,
        Err(SemanticError::StaleCacheRevision { .. }) | Err(SemanticError::StaleIdentity)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn oversized_mutation_breaks_generation_and_rejects_old_ingress() -> Result<(), Box<dyn Error>>
{
    let root = node(":1.34", "root")?;
    let root_address = root.object.clone();
    let mut items = Vec::with_capacity(xenoteer_atspi::MAX_MUTATION_ADDRESSES + 1);
    items.push(root);
    for index in 0..xenoteer_atspi::MAX_MUTATION_ADDRESSES {
        let mut child = node(":1.34", &format!("child-{index}"))?;
        child.parent = Some(root_address.clone());
        items.push(child);
    }
    let control = FakeControl::new(vec![Ok(items), Ok(Vec::new())]);
    let observation = control.clone();
    let mut config = test_config();
    config.cache_limits.max_nodes = xenoteer_atspi::MAX_MUTATION_ADDRESSES + 2;
    let spawned = spawn_atspi_actor(true, config, control)?;
    wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy
            && health.cached_nodes == xenoteer_atspi::MAX_MUTATION_ADDRESSES + 1
    })
    .await?;
    let old_ingress = observation.latest_ingress()?;
    assert_eq!(
        old_ingress.offer(BackendEvent::Cache(CacheEvent::Remove(root_address))),
        EventOfferResult::Accepted
    );
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.accessibility_generation >= 2
    })
    .await?;
    assert_eq!(health.accessibility_generation, 2);
    assert_eq!(
        old_ingress.offer(BackendEvent::ObjectChanged {
            source: None,
            kind: "stale-generation-event".to_owned(),
        }),
        EventOfferResult::Closed
    );
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn cancellation_after_dispatch_reports_unknown_effect() -> Result<(), Box<dyn Error>> {
    let mut item = node(":1.32", "focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]).stall_after_dispatch();
    let calls = Arc::clone(&control.semantic_calls);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let cached = page.nodes.first().ok_or("missing semantic node")?;
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let handle = spawned.handle.clone();
    let request = semantic_request(&health, cached, SemanticOperation::Focus);
    let task =
        tokio::spawn(async move { handle.execute_semantic(request, task_cancellation).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while calls.load(Ordering::SeqCst) == 0 {
        if tokio::time::Instant::now() >= deadline {
            return Err("semantic fake was never dispatched".into());
        }
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    assert_eq!(task.await?, Err(SemanticError::CancelledAfterDispatch));
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn shutdown_after_dispatch_preserves_actor_unknown_effect_classification()
-> Result<(), Box<dyn Error>> {
    let mut item = node(":1.35", "focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]).stall_after_dispatch();
    let dispatches = Arc::clone(&control.semantic_dispatches);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let mut request = semantic_request(
        &health,
        page.nodes.first().ok_or("missing semantic node")?,
        SemanticOperation::Focus,
    );
    request.deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let handle = spawned.handle.clone();
    let operation = tokio::spawn(async move {
        handle
            .execute_semantic(request, CancellationToken::new())
            .await
    });
    wait_for_atomic(&dispatches).await?;
    let shutdown = tokio::spawn(spawned.join.shutdown());
    let error = match operation.await? {
        Err(error) => error,
        Ok(_) => return Err("stalled dispatched call unexpectedly succeeded".into()),
    };
    assert_eq!(error, SemanticError::DeadlineAfterDispatch);
    assert!(!error.effect_definitely_not_dispatched());
    assert_eq!(shutdown.await?, AtspiActorExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn reply_loss_after_dispatch_is_terminal_unknown_effect() -> Result<(), Box<dyn Error>> {
    let mut item = node(":1.36", "focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]).panic_after_dispatch();
    let dispatches = Arc::clone(&control.semantic_dispatches);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let request = semantic_request(
        &health,
        page.nodes.first().ok_or("missing semantic node")?,
        SemanticOperation::Focus,
    );
    let result = spawned
        .handle
        .execute_semantic(request, CancellationToken::new())
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => return Err("panicked actor unexpectedly returned success".into()),
    };
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(error, SemanticError::ReplyLostAfterAdmission);
    assert!(!error.effect_definitely_not_dispatched());
    assert_eq!(spawned.join.wait().await, AtspiActorExit::Panicked);
    Ok(())
}

#[tokio::test]
async fn ingress_event_during_semantic_preflight_prevents_dispatch() -> Result<(), Box<dyn Error>> {
    let mut item = node(":1.37", "focus")?;
    item.interfaces.push("org.a11y.atspi.Component".to_owned());
    let control = FakeControl::new(vec![Ok(vec![item])]).wait_in_preflight();
    let observation = control.clone();
    let started = Arc::clone(&control.preflight_started);
    let release = Arc::clone(&control.preflight_release);
    let dispatches = Arc::clone(&control.semantic_dispatches);
    let spawned = spawn_atspi_actor(true, test_config(), control)?;
    let health = wait_for_health(&spawned.handle, |health| {
        health.state == AtspiActorState::Healthy && health.cached_nodes == 1
    })
    .await?;
    let page = spawned
        .handle
        .cache_page(
            Some(health.accessibility_generation),
            Some(health.cache_revision),
            None,
            CancellationToken::new(),
        )
        .await?;
    let handle = spawned.handle.clone();
    let request = semantic_request(
        &health,
        page.nodes.first().ok_or("missing semantic node")?,
        SemanticOperation::Focus,
    );
    let operation = tokio::spawn(async move {
        handle
            .execute_semantic(request, CancellationToken::new())
            .await
    });
    wait_for_atomic(&started).await?;
    assert_eq!(
        observation
            .latest_ingress()?
            .offer(BackendEvent::ObjectChanged {
                source: None,
                kind: "during-preflight".to_owned(),
            }),
        EventOfferResult::Accepted
    );
    release.notify_waiters();
    let error = match operation.await? {
        Err(error) => error,
        Ok(_) => return Err("changed ingress epoch unexpectedly dispatched".into()),
    };
    assert!(matches!(error, SemanticError::Backend(_)));
    assert!(error.effect_definitely_not_dispatched());
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(spawned.join.shutdown().await, AtspiActorExit::Stopped);
    Ok(())
}

async fn wait_for_atomic(value: &AtomicUsize) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while value.load(Ordering::SeqCst) == 0 {
        if tokio::time::Instant::now() >= deadline {
            return Err("atomic observation deadline exceeded".into());
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}
