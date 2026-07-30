use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use xenoteer_atspi::{
    ActionEvidence, AtspiActorEvent, AtspiActorExit, AtspiActorState, AtspiBackend,
    AtspiBackendConnector, BackendEvent, BackendEventIngress, BackendFailure, BackendFailureKind,
    BackendFuture, BackendObservationRequest, BackendSemanticRequest, CacheEvent, CacheLimits,
    EventOfferResult, NormalizedCacheItem, ObjectAddress, SelectionRangeEvidence,
    SemanticDispatchMarker, SemanticEvidence, SemanticObservationEvidence, SemanticOperation,
    SemanticRect, SemanticValueEvidence, TextProtection, TextReadbackEvidence,
    TextVerificationMode,
};
use xenoteer_core::{Config, ConfigOverrides};
use xenoteer_protocol::{
    AccessibilityQueryLimits, Command, CoordinateSpace, DesktopGeneration, DesktopId,
    EditableTextSelectionPolicy, ElementActionOperation, ElementActionTarget, ElementFocusCommand,
    ElementInsertTextCommand, ElementInvokeCommand, ElementListRequest, ElementOrder,
    ElementPostcondition, ElementScope, ElementScrollAlignment, ElementScrollCommand,
    ElementScrollTarget, ElementSelectionCommand, ElementSelectionOperation, ElementSetTextCommand,
    ElementSetValueCommand, ElementSnapshotExpansion, ElementStringMatch, ElementWaitPredicate,
    NormalizedEvent, Rect, SemanticTextInput, SemanticTextInsertOptions,
    SemanticTextInsertionPoint,
};
use xenoteer_server::AccessibilityPlaneError;

use crate::accessibility_events::AccessibilityEventPublisher;
use crate::accessibility_runtime::{
    AccessibilityRuntimeConfig, AccessibilityRuntimeReader, MirrorCursor,
    object_event_fence_generation_for_test, process_event_for_test,
    spawn_accessibility_runtime_with_connector,
    spawn_accessibility_runtime_with_connector_and_event_sink,
};
use crate::observation_plane::{WindowEventSink, WindowEventSinkError};
use crate::semantic_actions::{
    SemanticActionFailure, execute_semantic_action,
    execute_semantic_action_with_pre_observation_hook, execute_semantic_text_insert,
    execute_semantic_text_insert_with_pre_observation_hook, require_supported_postcondition,
};

fn state_words(states: &[u32]) -> Vec<u32> {
    let Some(maximum) = states.iter().copied().max() else {
        return Vec::new();
    };
    let mut words = vec![0_u32; maximum as usize / u32::BITS as usize + 1];
    for state in states {
        words[*state as usize / u32::BITS as usize] |=
            1_u32 << (*state as usize % u32::BITS as usize);
    }
    words
}

type BootstrapPlan = Result<Vec<NormalizedCacheItem>, BackendFailure>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeSemanticFailure {
    ProtocolBeforeDispatch,
    ProtocolAfterDispatch,
}

#[derive(Debug, Default)]
struct RecordingAccessibilityEventSink {
    events: Mutex<Vec<NormalizedEvent>>,
    resyncs: AtomicUsize,
}

impl RecordingAccessibilityEventSink {
    fn snapshot(&self) -> Result<Vec<NormalizedEvent>, Box<dyn Error>> {
        Ok(self
            .events
            .lock()
            .map_err(|_| "accessibility event sink lock poisoned")?
            .clone())
    }

    fn resyncs(&self) -> usize {
        self.resyncs.load(Ordering::SeqCst)
    }
}

impl WindowEventSink for RecordingAccessibilityEventSink {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        self.events
            .lock()
            .map_err(|_| WindowEventSinkError::Closed)?
            .push(event);
        Ok(())
    }

    fn require_resync(&self) {
        self.resyncs.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug)]
struct FakeConnector {
    plans: Arc<Mutex<VecDeque<BootstrapPlan>>>,
    ingresses: Arc<Mutex<Vec<BackendEventIngress>>>,
    connections: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    semantic_attempts: Arc<AtomicUsize>,
    semantic_calls: Arc<Mutex<Vec<&'static str>>>,
    semantic_pre_dispatch_conflicts: Arc<AtomicUsize>,
    semantic_failures: Arc<Mutex<VecDeque<FakeSemanticFailure>>>,
    exact_text_matches: Arc<Mutex<VecDeque<Option<bool>>>>,
}

impl FakeConnector {
    fn new(plans: Vec<BootstrapPlan>) -> Self {
        Self {
            plans: Arc::new(Mutex::new(VecDeque::from(plans))),
            ingresses: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            semantic_attempts: Arc::new(AtomicUsize::new(0)),
            semantic_calls: Arc::new(Mutex::new(Vec::new())),
            semantic_pre_dispatch_conflicts: Arc::new(AtomicUsize::new(0)),
            semantic_failures: Arc::new(Mutex::new(VecDeque::new())),
            exact_text_matches: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn with_semantic_pre_dispatch_conflicts(self, conflicts: usize) -> Self {
        self.semantic_pre_dispatch_conflicts
            .store(conflicts, Ordering::SeqCst);
        self
    }

    fn with_semantic_failures(
        self,
        failures: impl IntoIterator<Item = FakeSemanticFailure>,
    ) -> Self {
        if let Ok(mut configured) = self.semantic_failures.lock() {
            configured.extend(failures);
        }
        self
    }

    fn with_exact_text_matches(self, matches: impl IntoIterator<Item = Option<bool>>) -> Self {
        if let Ok(mut configured) = self.exact_text_matches.lock() {
            configured.extend(matches);
        }
        self
    }

    fn latest_ingress(&self) -> Result<BackendEventIngress, Box<dyn Error>> {
        self.ingresses
            .lock()
            .map_err(|_| "fake ingress lock poisoned")?
            .last()
            .cloned()
            .ok_or_else(|| "actor has not attempted a connection".into())
    }

    fn semantic_calls(&self) -> Result<Vec<&'static str>, Box<dyn Error>> {
        Ok(self
            .semantic_calls
            .lock()
            .map_err(|_| "fake semantic-call lock poisoned")?
            .clone())
    }

    fn semantic_attempts(&self) -> usize {
        self.semantic_attempts.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct FakeBackend {
    bootstrap: Option<BootstrapPlan>,
    ingress: BackendEventIngress,
    shutdowns: Arc<AtomicUsize>,
    semantic_attempts: Arc<AtomicUsize>,
    semantic_calls: Arc<Mutex<Vec<&'static str>>>,
    semantic_pre_dispatch_conflicts: Arc<AtomicUsize>,
    semantic_failures: Arc<Mutex<VecDeque<FakeSemanticFailure>>>,
    exact_text_matches: Arc<Mutex<VecDeque<Option<bool>>>>,
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
                "bootstrap called twice",
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
        request: BackendSemanticRequest,
        dispatch: SemanticDispatchMarker,
    ) -> BackendFuture<'_, Result<SemanticEvidence, BackendFailure>> {
        let ingress = self.ingress.clone();
        let semantic_attempts = Arc::clone(&self.semantic_attempts);
        let calls = Arc::clone(&self.semantic_calls);
        let conflicts = Arc::clone(&self.semantic_pre_dispatch_conflicts);
        let failures = Arc::clone(&self.semantic_failures);
        let exact_text_matches = Arc::clone(&self.exact_text_matches);
        Box::pin(async move {
            semantic_attempts.fetch_add(1, Ordering::SeqCst);
            if conflicts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
                && ingress.offer(BackendEvent::ObjectChanged {
                    source: None,
                    kind: "semantic-pre-dispatch-conflict".to_owned(),
                }) != EventOfferResult::Accepted
            {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "fake semantic ingress conflict was not accepted",
                ));
            }
            let failure = failures
                .lock()
                .map_err(|_| {
                    BackendFailure::new(
                        BackendFailureKind::Protocol,
                        "fake semantic-failure lock poisoned",
                    )
                })?
                .pop_front();
            if failure == Some(FakeSemanticFailure::ProtocolBeforeDispatch) {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "deterministic generic pre-dispatch protocol failure",
                ));
            }
            request.dispatch_permit.ensure_current()?;
            dispatch.mark_dispatched();
            let (kind, evidence) = match request.operation {
                SemanticOperation::Invoke(_) => (
                    "invoke",
                    SemanticEvidence::Action {
                        accepted: true,
                        invoked_index: 0,
                        actions: vec![ActionEvidence {
                            index: 0,
                            name: "activate".to_owned(),
                            description: String::new(),
                            keybinding: String::new(),
                        }],
                    },
                ),
                SemanticOperation::Focus => (
                    "focus",
                    SemanticEvidence::Focus {
                        accepted: true,
                        focused: true,
                    },
                ),
                SemanticOperation::SetValue(value) => (
                    "set_value",
                    SemanticEvidence::Value {
                        current: value,
                        minimum: 0.0,
                        maximum: 100.0,
                        minimum_increment: 1.0,
                    },
                ),
                SemanticOperation::Selection(_) => (
                    "selection",
                    SemanticEvidence::Selection {
                        accepted: true,
                        selected_children: 1,
                        addressed_child_selected: Some(true),
                    },
                ),
                SemanticOperation::SetText {
                    text, verification, ..
                } => (
                    "set_text",
                    SemanticEvidence::Text {
                        accepted: true,
                        before: text_evidence(2),
                        after: text_evidence(text.character_count()),
                        exact_match: fake_exact_match(&exact_text_matches, verification)?,
                    },
                ),
                SemanticOperation::InsertText {
                    text, verification, ..
                } => (
                    "insert_text",
                    SemanticEvidence::Text {
                        accepted: true,
                        before: text_evidence(2),
                        after: text_evidence(2_u32.saturating_add(text.character_count())),
                        exact_match: fake_exact_match(&exact_text_matches, verification)?,
                    },
                ),
                SemanticOperation::Scroll(_) | SemanticOperation::ScrollToPoint { .. } => (
                    "scroll",
                    SemanticEvidence::Scroll {
                        accepted: true,
                        before: SemanticRect {
                            x: 10,
                            y: 20,
                            width: 30,
                            height: 40,
                        },
                        after: SemanticRect {
                            x: 10,
                            y: 10,
                            width: 30,
                            height: 40,
                        },
                    },
                ),
            };
            calls
                .lock()
                .map_err(|_| {
                    BackendFailure::new(BackendFailureKind::Protocol, "semantic lock poisoned")
                })?
                .push(kind);
            if failure == Some(FakeSemanticFailure::ProtocolAfterDispatch) {
                return Err(BackendFailure::new(
                    BackendFailureKind::Protocol,
                    "deterministic post-dispatch protocol failure",
                ));
            }
            Ok(evidence)
        })
    }

    fn observe_exact(
        &mut self,
        request: BackendObservationRequest,
    ) -> BackendFuture<'_, Result<SemanticObservationEvidence, BackendFailure>> {
        Box::pin(async move {
            request.read_permit.ensure_current()?;
            Ok(SemanticObservationEvidence {
                identity_fingerprint: request.expected_identity,
                parent: None,
                index_in_parent: request.expected_index_in_parent,
                role: request.expected_role,
                states: state_words(&[7, 8, 11, 24, 25, 30]),
                interfaces: semantic_interfaces(),
                bounds: Some(SemanticRect {
                    x: 10,
                    y: 20,
                    width: 30,
                    height: 40,
                }),
                top_level: Some(request.object),
                application_pid: Some(4242),
                value: Some(SemanticValueEvidence {
                    current: 2.0,
                    minimum: 0.0,
                    maximum: 100.0,
                    minimum_increment: 1.0,
                }),
                text: Some(text_evidence(2)),
                selected_children: Some(0),
            })
        })
    }
}

impl AtspiBackendConnector for FakeConnector {
    type Backend = FakeBackend;

    fn connect(
        &mut self,
        ingress: BackendEventIngress,
        _cache_limits: CacheLimits,
    ) -> BackendFuture<'_, Result<Self::Backend, BackendFailure>> {
        self.connections.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut ingresses) = self.ingresses.lock() {
            ingresses.push(ingress.clone());
        }
        let plan = self
            .plans
            .lock()
            .ok()
            .and_then(|mut plans| plans.pop_front());
        let shutdowns = Arc::clone(&self.shutdowns);
        let semantic_attempts = Arc::clone(&self.semantic_attempts);
        let semantic_calls = Arc::clone(&self.semantic_calls);
        let semantic_pre_dispatch_conflicts = Arc::clone(&self.semantic_pre_dispatch_conflicts);
        let semantic_failures = Arc::clone(&self.semantic_failures);
        let exact_text_matches = Arc::clone(&self.exact_text_matches);
        Box::pin(async move {
            match plan {
                Some(bootstrap) => Ok(FakeBackend {
                    bootstrap: Some(bootstrap),
                    ingress,
                    shutdowns,
                    semantic_attempts,
                    semantic_calls,
                    semantic_pre_dispatch_conflicts,
                    semantic_failures,
                    exact_text_matches,
                }),
                None => Err(BackendFailure::new(
                    BackendFailureKind::Connection,
                    "fake accessibility backend missing",
                )),
            }
        })
    }
}

fn fake_exact_match(
    matches: &Mutex<VecDeque<Option<bool>>>,
    verification: TextVerificationMode,
) -> Result<Option<bool>, BackendFailure> {
    let configured = matches
        .lock()
        .map_err(|_| {
            BackendFailure::new(
                BackendFailureKind::Protocol,
                "fake exact-text match lock poisoned",
            )
        })?
        .pop_front();
    if let Some(configured) = configured {
        return Ok(configured);
    }
    match verification {
        TextVerificationMode::LengthOnly => Ok(None),
        TextVerificationMode::Exact => Ok(Some(true)),
    }
}

fn text_evidence(character_count: u32) -> TextReadbackEvidence {
    TextReadbackEvidence {
        character_count,
        caret_offset: i32::try_from(character_count).unwrap_or(i32::MAX),
        selections: Vec::<SelectionRangeEvidence>::new(),
    }
}

fn semantic_interfaces() -> Vec<String> {
    [
        "org.a11y.atspi.Accessible",
        "org.a11y.atspi.Action",
        "org.a11y.atspi.Component",
        "org.a11y.atspi.Value",
        "org.a11y.atspi.Selection",
        "org.a11y.atspi.EditableText",
        "org.a11y.atspi.Text",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn app(bus: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    Ok(NormalizedCacheItem {
        object: ObjectAddress::new(bus, "/test/app")?,
        application: ObjectAddress::new(bus, "/test/app")?,
        parent: None,
        index_in_parent: None,
        child_count: None,
        legacy_children: Vec::new(),
        interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
        name: "application".to_owned(),
        description: String::new(),
        role: 0,
        text_protection: TextProtection::Unprotected,
        states: Vec::new(),
    })
}

fn child(bus: &str, suffix: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    Ok(NormalizedCacheItem {
        object: ObjectAddress::new(bus, format!("/test/{suffix}"))?,
        application: ObjectAddress::new(bus, "/test/app")?,
        parent: Some(ObjectAddress::new(bus, "/test/app")?),
        index_in_parent: None,
        child_count: None,
        legacy_children: Vec::new(),
        interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
        name: suffix.to_owned(),
        description: String::new(),
        role: 0,
        text_protection: TextProtection::Unprotected,
        states: Vec::new(),
    })
}

fn semantic_child(bus: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    let mut item = child(bus, "semantic")?;
    item.child_count = Some(1);
    item.interfaces = semantic_interfaces();
    item.role = 79;
    item.states = state_words(&[7, 8, 11, 24, 25, 30]);
    Ok(item)
}

fn protected_semantic_child(bus: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    let mut item = semantic_child(bus)?;
    item.name = "must-not-project-password-name".to_owned();
    item.description = "must-not-project-password-description".to_owned();
    item.role = 40;
    item.text_protection = TextProtection::Protected;
    Ok(item)
}

fn unknown_semantic_child(bus: &str) -> Result<NormalizedCacheItem, Box<dyn Error>> {
    let mut item = semantic_child(bus)?;
    item.role = u32::MAX;
    item.text_protection = TextProtection::Unknown;
    Ok(item)
}

fn configured(file: Option<&str>) -> Result<AccessibilityRuntimeConfig, Box<dyn Error>> {
    let config = Config::load(
        file,
        std::iter::empty::<(String, String)>(),
        &ConfigOverrides::default(),
    )?;
    Ok(AccessibilityRuntimeConfig::from_config(
        config.accessibility(),
    ))
}

async fn wait_for_runtime(
    reader: &AccessibilityRuntimeReader,
    predicate: impl Fn(crate::accessibility_runtime::AccessibilityRuntimeSnapshot) -> bool,
) -> Result<crate::accessibility_runtime::AccessibilityRuntimeSnapshot, Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = reader.snapshot();
        if predicate(snapshot) {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("accessibility runtime wait expired: {snapshot:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn list_request(desktop_id: DesktopId, generation: DesktopGeneration) -> ElementListRequest {
    ElementListRequest {
        desktop_id,
        desktop_generation: generation,
        scope: ElementScope::Desktop,
        order: ElementOrder::ObjectPathAscending,
        limit: 100,
        cursor: None,
        expansion: ElementSnapshotExpansion {
            actions: false,
            value: false,
            text_metadata: false,
            text_content: false,
            attributes: false,
            relations: false,
            component: false,
        },
        limits: AccessibilityQueryLimits::default(),
    }
}

#[test]
fn default_queue_capacities_report_each_layer_independently() -> Result<(), Box<dyn Error>> {
    let config = configured(None)?;
    assert_eq!(config.requested_event_capacity, 4_096);
    assert_eq!(config.actor.backend_event_capacity, 512);
    assert_eq!(config.actor.event_capacity, 512);
    assert_eq!(config.decoded_event_capacity, 512);
    assert_eq!(config.raw_signal_queue_capacity, 5);
    assert_ne!(
        config.raw_signal_queue_capacity,
        config.decoded_event_capacity
    );
    assert_eq!(
        config
            .decoded_event_capacity
            .checked_mul(config.actor.cache_limits.max_item_bytes),
        Some(config.actor.cache_limits.max_total_bytes)
    );
    assert_eq!(config.actor.validate(), Ok(config.actor));
    Ok(())
}

#[test]
fn operator_query_and_snapshot_limits_map_into_the_plane() -> Result<(), Box<dyn Error>> {
    let config = configured(Some(
        "[accessibility]\n\
         max_nodes_per_query = 7\n\
         max_selector_depth = 3\n\
         max_query_matches = 2\n\
         max_snapshot_nodes = 1\n\
         max_snapshot_bytes = 1024\n\
         proxy_timeout_ms = 10\n\
         query_timeout_ms = 11",
    ))?;
    let plane = config.plane_config();
    assert_eq!(plane.max_nodes_per_query, 7);
    assert_eq!(plane.max_selector_depth, 3);
    assert_eq!(plane.max_query_matches, 2);
    assert_eq!(plane.max_snapshot_nodes, 1);
    assert_eq!(plane.max_snapshot_bytes, 1_024);
    assert_eq!(plane.query_timeout_ms, 11);
    Ok(())
}

#[test]
fn container_accessibility_bus_matches_the_pinned_zbus_message_limit() {
    let bus_config = include_str!("../../../container/rootfs/etc/at-spi2/accessibility.conf");
    for limit in [
        "max_incoming_bytes",
        "max_outgoing_bytes",
        "max_message_size",
    ] {
        assert!(bus_config.contains(&format!("<limit name=\"{limit}\">134217728</limit>")));
    }
}

#[tokio::test]
async fn initial_bootstrap_and_incremental_mutation_become_atomically_readable()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.10")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    let reader = runtime.reader();
    let ready = wait_for_runtime(&reader, |snapshot| snapshot.mirror_ready).await?;
    assert_eq!(ready.accessibility_generation, 1);
    assert_eq!(ready.cache_revision, 1);

    assert_eq!(
        connector
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(child(
                ":1.10", "button",
            )?)))),
        EventOfferResult::Accepted
    );
    let incremental = wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready && snapshot.cache_revision == 2
    })
    .await?;
    assert_eq!(incremental.accessibility_generation, 1);
    let page = runtime
        .plane()
        .list_for(
            "test-principal",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("accessibility list failed: {error:?}"))?;
    assert_eq!(page.elements.len(), 2);

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn object_events_are_suppressed_while_the_mirror_is_rebuilding() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.11")?, child(":1.11", "button")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector,
    )?;
    let ready = wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let plane = runtime.plane();
    let sink = Arc::new(RecordingAccessibilityEventSink::default());
    let publisher = AccessibilityEventPublisher::new(sink.clone());
    let mut cursor =
        MirrorCursor::mirrored_for_test(ready.accessibility_generation, ready.cache_revision);

    let covered_revision = ready
        .cache_revision
        .checked_sub(1)
        .ok_or("test cache revision did not advance during bootstrap")?;
    process_event_for_test(
        AtspiActorEvent::ObjectChanged {
            accessibility_generation: ready.accessibility_generation,
            cache_revision: covered_revision,
            source: Some(ObjectAddress::new(":1.11", "/test/button")?),
            kind: "object.state_changed".to_owned(),
        },
        plane.as_ref(),
        &mut cursor,
        0,
        &publisher,
    )
    .await;
    assert!(sink.snapshot()?.is_empty());
    assert_eq!(sink.resyncs(), 0);

    // A missing source is still a genuine publication gap while the mirror is
    // ready; suppression begins only after a rebuild fence is already active.
    process_event_for_test(
        AtspiActorEvent::ObjectChanged {
            accessibility_generation: ready.accessibility_generation,
            cache_revision: ready.cache_revision,
            source: None,
            kind: "object.selection_changed".to_owned(),
        },
        plane.as_ref(),
        &mut cursor,
        0,
        &publisher,
    )
    .await;
    assert_eq!(sink.resyncs(), 1);

    cursor.invalidate_for_test(true);

    process_event_for_test(
        AtspiActorEvent::ObjectChanged {
            accessibility_generation: ready.accessibility_generation,
            cache_revision: ready.cache_revision,
            source: Some(ObjectAddress::new(":1.11", "/test/button")?),
            kind: "object.state_changed".to_owned(),
        },
        plane.as_ref(),
        &mut cursor,
        0,
        &publisher,
    )
    .await;
    process_event_for_test(
        AtspiActorEvent::ObjectChanged {
            accessibility_generation: ready.accessibility_generation,
            cache_revision: ready.cache_revision.saturating_add(1),
            source: None,
            kind: "object.selection_changed".to_owned(),
        },
        plane.as_ref(),
        &mut cursor,
        0,
        &publisher,
    )
    .await;

    assert!(cursor.rebuild_pending_for_test());
    assert!(sink.snapshot()?.is_empty());
    assert_eq!(sink.resyncs(), 1);
    let page = plane
        .list_for(
            "suppressed-object-event-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("suppressed event changed the ready plane: {error:?}"))?;
    assert_eq!(page.elements.len(), 2);

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn forward_object_event_revision_fences_once_then_suppresses_the_flood()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.13")?, child(":1.13", "button")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector,
    )?;
    let ready = wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let plane = runtime.plane();
    let sink = Arc::new(RecordingAccessibilityEventSink::default());
    let publisher = AccessibilityEventPublisher::new(sink.clone());
    let mut cursor =
        MirrorCursor::mirrored_for_test(ready.accessibility_generation, ready.cache_revision);
    let jumped_generation = ready
        .accessibility_generation
        .checked_add(4)
        .ok_or("test accessibility generation overflow")?;
    assert_eq!(
        object_event_fence_generation_for_test(&cursor, jumped_generation, ready.cache_revision),
        Some(jumped_generation)
    );
    let forward_revision = ready
        .cache_revision
        .checked_add(1)
        .ok_or("test cache revision overflow")?;
    assert_eq!(
        object_event_fence_generation_for_test(
            &cursor,
            ready.accessibility_generation,
            forward_revision,
        ),
        Some(ready.accessibility_generation)
    );
    let event = AtspiActorEvent::ObjectChanged {
        accessibility_generation: ready.accessibility_generation,
        cache_revision: forward_revision,
        source: Some(ObjectAddress::new(":1.13", "/test/button")?),
        kind: "object.state_changed".to_owned(),
    };

    process_event_for_test(event.clone(), plane.as_ref(), &mut cursor, 0, &publisher).await;
    process_event_for_test(event, plane.as_ref(), &mut cursor, 0, &publisher).await;
    process_event_for_test(
        AtspiActorEvent::ObjectChanged {
            accessibility_generation: ready.accessibility_generation,
            cache_revision: forward_revision,
            source: None,
            kind: "object.selection_changed".to_owned(),
        },
        plane.as_ref(),
        &mut cursor,
        0,
        &publisher,
    )
    .await;

    assert!(cursor.rebuild_pending_for_test());
    let events = sink.snapshot()?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic.as_str(), "accessibility.resync_required");
    assert_eq!(events[0].payload["resync_reason"], "event_gap");
    assert_eq!(sink.resyncs(), 0);
    assert!(matches!(
        plane
            .list_for(
                "forward-object-event-test",
                list_request(desktop_id, desktop_generation),
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn application_invalidation_cache_change_precedes_marker_without_duplicate_removals()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.12")?, child(":1.12", "button")?])]);
    let sink = Arc::new(RecordingAccessibilityEventSink::default());
    let runtime = spawn_accessibility_runtime_with_connector_and_event_sink(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
        sink.clone(),
    )?;
    let reader = runtime.reader();
    wait_for_runtime(&reader, |snapshot| snapshot.mirror_ready).await?;

    assert_eq!(
        connector
            .latest_ingress()?
            .offer(BackendEvent::Cache(CacheEvent::InvalidateApplication(
                ":1.12".to_owned(),
            ))),
        EventOfferResult::Accepted
    );
    wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready && snapshot.cache_revision == 2
    })
    .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let events = loop {
        let events = sink.snapshot()?;
        if events.len() >= 2 {
            break events;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("application invalidation removal events were not published".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // Give the paired owner-lifetime marker time to pass through the runtime;
    // it is already covered by the preceding CacheChanged revision and must
    // not manufacture a second set of cache-removal events.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let events_after_marker = sink.snapshot()?;
    assert_eq!(events_after_marker.len(), 2);
    assert_eq!(events, events_after_marker);
    for event in events_after_marker {
        assert_eq!(event.topic.as_str(), "accessibility.element_removed");
        assert_eq!(event.payload["kind"], "cache_removed");
        assert_eq!(event.payload["atspi_generation"], "1");
    }

    let page = runtime
        .plane()
        .list_for(
            "test-principal",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("accessibility list failed: {error:?}"))?;
    assert!(page.elements.is_empty());
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn bootstrap_only_cache_reaches_actor_for_all_seven_semantic_commands()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.15")?, semantic_child(":1.15")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "semantic-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("semantic list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing semantic element")?
        .snapshot
        .element;
    let semantic = runtime.semantic_runtime();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let commands = vec![
        Command::ElementInvoke(ElementInvokeCommand {
            element: element.clone(),
            action: ElementActionTarget::Index { index: 0 },
            allow_disabled: false,
            postcondition: None,
        }),
        Command::ElementFocus(ElementFocusCommand {
            element: element.clone(),
            require_window_focus_correlation: false,
            postcondition: None,
        }),
        Command::ElementSetValue(ElementSetValueCommand {
            element: element.clone(),
            value: 5.0,
            tolerance: None,
            postcondition: None,
        }),
        Command::ElementSelection(ElementSelectionCommand {
            element: element.clone(),
            operation: ElementSelectionOperation::SelectChild { index: 0 },
            postcondition: None,
        }),
        Command::ElementSetText(ElementSetTextCommand {
            element: element.clone(),
            text: SemanticTextInput::new("abc")?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        Command::ElementInsertText(ElementInsertTextCommand {
            element: element.clone(),
            offset: 1,
            text: SemanticTextInput::new("x")?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        Command::ElementScroll(ElementScrollCommand {
            element: element.clone(),
            target: ElementScrollTarget::Alignment {
                alignment: ElementScrollAlignment::Anywhere,
            },
            postcondition: None,
        }),
    ];
    let expected = [
        ElementActionOperation::Invoke,
        ElementActionOperation::Focus,
        ElementActionOperation::SetValue,
        ElementActionOperation::Selection,
        ElementActionOperation::SetText,
        ElementActionOperation::InsertText,
        ElementActionOperation::Scroll,
    ];
    for (command, operation) in commands.into_iter().zip(expected) {
        let result = execute_semantic_action(
            &semantic,
            command,
            deadline,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| format!("semantic action failed: {error:?}"))?;
        assert_eq!(result.operation, operation);
    }
    assert_eq!(
        connector.semantic_calls()?,
        vec![
            "invoke",
            "focus",
            "set_value",
            "selection",
            "set_text",
            "insert_text",
            "scroll"
        ]
    );
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn semantic_action_retries_forward_observation_revision_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.151")?, semantic_child(":1.151")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "semantic-forward-revision-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("semantic list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing semantic element")?
        .snapshot
        .element;
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&hook_calls);
    let hook_connector = connector.clone();
    let reader = runtime.reader();
    let unrelated = app(":1.152")?;
    let result = execute_semantic_action_with_pre_observation_hook(
        &runtime.semantic_runtime(),
        Command::ElementSetValue(ElementSetValueCommand {
            element,
            value: 5.0,
            tolerance: None,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
        move || {
            let invocation = calls.fetch_add(1, Ordering::SeqCst);
            let connector = hook_connector.clone();
            let reader = reader.clone();
            let unrelated = unrelated.clone();
            async move {
                if invocation == 0 {
                    let ingress = connector.latest_ingress().map_err(|_| {
                        SemanticActionFailure::PlaneBefore(AccessibilityPlaneError::Internal)
                    })?;
                    if ingress.offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(unrelated))))
                        != EventOfferResult::Accepted
                    {
                        return Err(SemanticActionFailure::PlaneBefore(
                            AccessibilityPlaneError::Internal,
                        ));
                    }
                    wait_for_runtime(&reader, |snapshot| {
                        snapshot.mirror_ready && snapshot.cache_revision >= 2
                    })
                    .await
                    .map_err(|_| {
                        SemanticActionFailure::PlaneBefore(AccessibilityPlaneError::Internal)
                    })?;
                }
                Ok(())
            }
        },
    )
    .await
    .map_err(|error| format!("forward-revision semantic action failed: {error:?}"))?;
    assert_eq!(result.operation, ElementActionOperation::SetValue);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert_eq!(connector.semantic_calls()?, vec!["set_value"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn unsupported_postconditions_fail_before_semantic_dispatch_while_supported_predicates_work()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.17")?, semantic_child(":1.17")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "semantic-postcondition-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("semantic list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing semantic element")?
        .snapshot
        .element;
    let semantic = runtime.semantic_runtime();

    for predicate in [ElementWaitPredicate::Text {
        matcher: ElementStringMatch::Exact {
            value: "never-read".to_owned(),
            case_sensitive: true,
        },
    }] {
        let result = execute_semantic_action(
            &semantic,
            Command::ElementScroll(ElementScrollCommand {
                element: element.clone(),
                target: ElementScrollTarget::Alignment {
                    alignment: ElementScrollAlignment::Anywhere,
                },
                postcondition: Some(ElementPostcondition {
                    predicate,
                    timeout_ms: 100,
                    allow_poll_fallback: false,
                }),
            }),
            tokio::time::Instant::now() + Duration::from_secs(2),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => return Err("unsupported postcondition unexpectedly dispatched".into()),
        };
        assert!(matches!(
            error,
            SemanticActionFailure::VerificationUnsupported
        ));
        assert!(connector.semantic_calls()?.is_empty());
    }

    require_supported_postcondition(Some(&ElementPostcondition {
        predicate: ElementWaitPredicate::Geometry {
            coordinate_space: CoordinateSpace::AtspiScreen,
            intersects: Rect::new(0, 0, 100, 100)?,
        },
        timeout_ms: 100,
        allow_poll_fallback: false,
    }))
    .map_err(|error| format!("supported postcondition was rejected: {error:?}"))?;
    let supported = execute_semantic_action(
        &semantic,
        Command::ElementScroll(ElementScrollCommand {
            element,
            target: ElementScrollTarget::Alignment {
                alignment: ElementScrollAlignment::Anywhere,
            },
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("supported postcondition failed: {error:?}"))?;
    assert_eq!(supported.evidence.postcondition_satisfied, None);
    assert_eq!(connector.semantic_calls()?, vec!["scroll"]);

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn semantic_text_insert_resolves_live_caret_and_returns_length_only_evidence()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.16")?, semantic_child(":1.16")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "semantic-text-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("semantic text list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing semantic text element")?
        .snapshot
        .element;
    let evidence = execute_semantic_text_insert(
        &runtime.semantic_runtime(),
        element,
        "xyz".to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("semantic text insert failed: {error:?}"))?;
    assert_eq!(evidence.insertion_offset, 2);
    assert_eq!(evidence.character_count_before, 2);
    assert_eq!(evidence.character_count_after, 5);
    assert!(evidence.verified_length_only);
    assert_eq!(connector.semantic_calls()?, vec!["insert_text"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn unprotected_exact_unicode_set_and_insert_return_only_sanitized_success_evidence()
-> Result<(), Box<dyn Error>> {
    const SET_SECRET: &str = "é🦀a";
    const INSERT_SECRET: &str = "λ🧪";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.165")?, semantic_child(":1.165")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "exact-unicode-semantic-text-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("exact semantic text list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing exact semantic text element")?
        .snapshot
        .element;
    let semantic = runtime.semantic_runtime();

    let set = execute_semantic_action(
        &semantic,
        Command::ElementSetText(ElementSetTextCommand {
            element: element.clone(),
            text: SemanticTextInput::new(SET_SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: false,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("exact Unicode set failed: {error:?}"))?;
    assert_eq!(set.evidence.observed_text_length, Some(3));
    assert!(!set.evidence.protected_text_verified_by_length_only);
    assert!(!format!("{set:?}").contains(SET_SECRET));

    let direct_insert = execute_semantic_action(
        &semantic,
        Command::ElementInsertText(ElementInsertTextCommand {
            element: element.clone(),
            offset: 1,
            text: SemanticTextInput::new(INSERT_SECRET)?,
            selection: EditableTextSelectionPolicy::SelectInserted,
            verify_length_only: false,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("exact Unicode direct insert failed: {error:?}"))?;
    assert_eq!(direct_insert.evidence.observed_text_length, Some(4));
    assert!(
        !direct_insert
            .evidence
            .protected_text_verified_by_length_only
    );
    assert!(!format!("{direct_insert:?}").contains(INSERT_SECRET));

    let inserted = execute_semantic_text_insert(
        &semantic,
        element,
        INSERT_SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: false,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("exact Unicode text.insert failed: {error:?}"))?;
    assert_eq!(inserted.insertion_offset, 2);
    assert_eq!(inserted.character_count_before, 2);
    assert_eq!(inserted.character_count_after, 4);
    assert!(!inserted.verified_length_only);
    assert!(!format!("{inserted:?}").contains(INSERT_SECRET));
    assert_eq!(
        connector.semantic_calls()?,
        vec!["set_text", "insert_text", "insert_text"]
    );

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn exact_same_character_count_mismatch_fails_after_one_dispatch_without_secret_leak()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "🦀same-scalar-count";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.166")?, semantic_child(":1.166")?])])
        .with_exact_text_matches([Some(false)]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "exact-mismatch-semantic-text-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("exact mismatch list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing exact mismatch element")?
        .snapshot
        .element;
    let command = Command::ElementSetText(ElementSetTextCommand {
        element,
        text: SemanticTextInput::new(SECRET)?,
        selection: EditableTextSelectionPolicy::CollapseAfter,
        verify_length_only: false,
        postcondition: None,
    });
    assert!(!format!("{command:?}").contains(SECRET));
    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        command,
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::PostconditionFailed)
    ));
    assert!(!format!("{result:?}").contains(SECRET));
    assert_eq!(connector.semantic_calls()?, vec!["set_text"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn malformed_missing_exact_match_evidence_is_rejected_after_dispatch()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.167")?, semantic_child(":1.167")?])])
        .with_exact_text_matches([None]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "missing-exact-evidence-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("missing exact evidence list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing exact evidence element")?
        .snapshot
        .element;
    let result = execute_semantic_text_insert(
        &runtime.semantic_runtime(),
        element,
        "bounded-canary".to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: false,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::InvalidEvidence)
    ));
    assert!(!format!("{result:?}").contains("bounded-canary"));
    assert_eq!(connector.semantic_calls()?, vec!["insert_text"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn malformed_exact_match_evidence_on_length_only_is_rejected_after_dispatch()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.168")?, semantic_child(":1.168")?])])
        .with_exact_text_matches([Some(true)]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "unexpected-exact-evidence-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("unexpected exact evidence list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing unexpected exact evidence element")?
        .snapshot
        .element;
    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSetText(ElementSetTextCommand {
            element,
            text: SemanticTextInput::new("length-only-canary")?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::InvalidEvidence)
    ));
    assert!(!format!("{result:?}").contains("length-only-canary"));
    assert_eq!(connector.semantic_calls()?, vec!["set_text"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn secret_text_insert_retries_forward_observation_revision_before_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-forward-revision-secret";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.160")?,
        protected_semantic_child(":1.160")?,
    ])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "secret-forward-revision-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&hook_calls);
    let hook_connector = connector.clone();
    let reader = runtime.reader();
    let unrelated = app(":1.159")?;
    let result = execute_semantic_text_insert_with_pre_observation_hook(
        &runtime.semantic_runtime(),
        element,
        SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
        move || {
            let invocation = calls.fetch_add(1, Ordering::SeqCst);
            let connector = hook_connector.clone();
            let reader = reader.clone();
            let unrelated = unrelated.clone();
            async move {
                if invocation == 0 {
                    let ingress = connector.latest_ingress().map_err(|_| {
                        SemanticActionFailure::PlaneBefore(AccessibilityPlaneError::Internal)
                    })?;
                    if ingress.offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(unrelated))))
                        != EventOfferResult::Accepted
                    {
                        return Err(SemanticActionFailure::PlaneBefore(
                            AccessibilityPlaneError::Internal,
                        ));
                    }
                    wait_for_runtime(&reader, |snapshot| {
                        snapshot.mirror_ready && snapshot.cache_revision >= 2
                    })
                    .await
                    .map_err(|_| {
                        SemanticActionFailure::PlaneBefore(AccessibilityPlaneError::Internal)
                    })?;
                }
                Ok(())
            }
        },
    )
    .await
    .map_err(|error| format!("forward-revision secret insert failed: {error:?}"))?;
    let inserted_characters = u32::try_from(SECRET.chars().count())?;
    assert_eq!(result.character_count_before, 2);
    assert_eq!(result.character_count_after, 2 + inserted_characters);
    assert!(result.verified_length_only);
    assert!(!format!("{result:?}").contains(SECRET));
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert_eq!(connector.semantic_calls()?, vec!["insert_text"]);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn protected_set_text_retries_one_ingress_conflict_before_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-set-conflict";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1601")?,
        protected_semantic_child(":1.1601")?,
    ])])
    .with_semantic_pre_dispatch_conflicts(1);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-set-conflict-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSetText(ElementSetTextCommand {
            element,
            text: SemanticTextInput::new(SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("protected set after ingress conflict failed: {error:?}"))?;
    assert_eq!(result.operation, ElementActionOperation::SetText);
    assert_eq!(
        result.evidence.observed_text_length,
        Some(u32::try_from(SECRET.chars().count())?)
    );
    assert!(result.evidence.protected_text_verified_by_length_only);
    assert_eq!(connector.semantic_attempts(), 2);
    assert_eq!(connector.semantic_calls()?, vec!["set_text"]);
    assert!(!format!("{result:?}").contains(SECRET));
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn semantic_text_insert_retries_one_ingress_conflict_before_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-insert-conflict";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1602")?,
        protected_semantic_child(":1.1602")?,
    ])])
    .with_semantic_pre_dispatch_conflicts(1);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-insert-conflict-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_text_insert(
        &runtime.semantic_runtime(),
        element,
        SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("semantic insert after ingress conflict failed: {error:?}"))?;
    let inserted_characters = u32::try_from(SECRET.chars().count())?;
    assert_eq!(result.character_count_before, 2);
    assert_eq!(result.character_count_after, 2 + inserted_characters);
    assert!(result.verified_length_only);
    assert_eq!(connector.semantic_attempts(), 2);
    assert_eq!(connector.semantic_calls()?, vec!["insert_text"]);
    assert!(!format!("{result:?}").contains(SECRET));
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn generic_pre_dispatch_protocol_failure_is_terminal_without_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-generic-pre-dispatch";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1605")?,
        protected_semantic_child(":1.1605")?,
    ])])
    .with_semantic_failures([FakeSemanticFailure::ProtocolBeforeDispatch]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-generic-pre-dispatch-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSetText(ElementSetTextCommand {
            element,
            text: SemanticTextInput::new(SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::Backend(BackendFailure {
                kind: BackendFailureKind::Protocol,
                ..
            })
        ))
    ));
    assert_eq!(connector.semantic_attempts(), 1);
    assert!(connector.semantic_calls()?.is_empty());
    assert!(!format!("{result:?}").contains(SECRET));
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn post_dispatch_protocol_failure_is_terminal_with_unknown_outcome()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-post-dispatch";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1606")?,
        protected_semantic_child(":1.1606")?,
    ])])
    .with_semantic_failures([FakeSemanticFailure::ProtocolAfterDispatch]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-post-dispatch-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSetText(ElementSetTextCommand {
            element,
            text: SemanticTextInput::new(SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::BackendAfterDispatch(BackendFailure {
                kind: BackendFailureKind::Protocol,
                ..
            })
        ))
    ));
    assert_eq!(connector.semantic_attempts(), 1);
    assert_eq!(connector.semantic_calls()?, vec!["set_text"]);
    assert!(!format!("{result:?}").contains(SECRET));
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn protected_set_text_bounds_repeated_ingress_conflicts_without_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-set-repeated-conflict";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1603")?,
        protected_semantic_child(":1.1603")?,
    ])])
    .with_semantic_pre_dispatch_conflicts(2);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-set-repeated-conflict-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSetText(ElementSetTextCommand {
            element,
            text: SemanticTextInput::new(SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::PreDispatchConflict
        ))
    ));
    assert!(!format!("{result:?}").contains(SECRET));
    assert_eq!(connector.semantic_attempts(), 2);
    assert!(connector.semantic_calls()?.is_empty());
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn semantic_text_insert_bounds_repeated_ingress_conflicts_without_dispatch()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-insert-repeated-conflict";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.1604")?,
        protected_semantic_child(":1.1604")?,
    ])])
    .with_semantic_pre_dispatch_conflicts(2);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let element = runtime
        .plane()
        .list_for(
            "protected-insert-repeated-conflict-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot
        .element;

    let result = execute_semantic_text_insert(
        &runtime.semantic_runtime(),
        element,
        SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::Actor(
            xenoteer_atspi::SemanticError::PreDispatchConflict
        ))
    ));
    assert!(!format!("{result:?}").contains(SECRET));
    assert_eq!(connector.semantic_attempts(), 2);
    assert!(connector.semantic_calls()?.is_empty());
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn protected_direct_and_semantic_text_insert_use_only_length_evidence()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-protected-insert-secret";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.161")?,
        protected_semantic_child(":1.161")?,
    ])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "protected-semantic-text-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("protected semantic list failed: {error:?}"))?;
    let snapshot = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::PasswordText)
        .ok_or("missing protected semantic element")?
        .snapshot;
    assert!(snapshot.is_protected());
    assert!(snapshot.name.is_none());
    assert!(snapshot.description.is_none());
    assert!(snapshot.text.is_none());
    let element = snapshot.element;
    let semantic = runtime.semantic_runtime();

    let denied_command = Command::ElementInsertText(ElementInsertTextCommand {
        element: element.clone(),
        offset: 1,
        text: SemanticTextInput::new(SECRET)?,
        selection: EditableTextSelectionPolicy::CollapseAfter,
        verify_length_only: false,
        postcondition: None,
    });
    assert!(!format!("{denied_command:?}").contains(SECRET));
    let denied = execute_semantic_action(
        &semantic,
        denied_command,
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        denied,
        Err(SemanticActionFailure::VerificationUnsupported)
    ));
    assert!(!format!("{denied:?}").contains(SECRET));
    assert!(connector.semantic_calls()?.is_empty());

    let direct = execute_semantic_action(
        &semantic,
        Command::ElementInsertText(ElementInsertTextCommand {
            element: element.clone(),
            offset: 1,
            text: SemanticTextInput::new(SECRET)?,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("protected direct insert failed: {error:?}"))?;
    assert_eq!(direct.operation, ElementActionOperation::InsertText);
    let inserted_characters = u32::try_from(SECRET.chars().count())?;
    assert_eq!(
        direct.evidence.observed_text_length,
        Some(2 + inserted_characters)
    );
    assert!(direct.evidence.protected_text_verified_by_length_only);
    assert!(!format!("{direct:?}").contains(SECRET));

    let inserted = execute_semantic_text_insert(
        &semantic,
        element,
        SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("protected semantic text.insert failed: {error:?}"))?;
    assert_eq!(inserted.insertion_offset, 2);
    assert_eq!(inserted.character_count_before, 2);
    assert_eq!(inserted.character_count_after, 2 + inserted_characters);
    assert!(inserted.verified_length_only);
    assert!(!format!("{inserted:?}").contains(SECRET));
    assert_eq!(
        connector.semantic_calls()?,
        vec!["insert_text", "insert_text"]
    );

    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn unknown_protection_text_insert_fails_closed_and_keeps_errors_secret_free()
-> Result<(), Box<dyn Error>> {
    const SECRET: &str = "phase5-unknown-protection-secret";
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let connector = FakeConnector::new(vec![Ok(vec![
        app(":1.162")?,
        unknown_semantic_child(":1.162")?,
    ])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "unknown-protection-text-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("unknown-protection list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Unknown)
        .ok_or("missing unknown-protection element")?
        .snapshot
        .element;
    let semantic = runtime.semantic_runtime();
    let result = execute_semantic_text_insert(
        &semantic,
        element,
        SECRET.to_owned(),
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: false,
            postcondition: None,
        },
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::VerificationUnsupported)
    ));
    assert!(!format!("{result:?}").contains(SECRET));
    assert!(connector.semantic_calls()?.is_empty());
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn select_all_without_exact_child_count_fails_before_dispatch() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let mut target = semantic_child(":1.163")?;
    target.child_count = None;
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.163")?, target])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    wait_for_runtime(&runtime.reader(), |snapshot| snapshot.mirror_ready).await?;
    let page = runtime
        .plane()
        .list_for(
            "select-all-child-count-test",
            list_request(desktop_id, desktop_generation),
        )
        .await
        .map_err(|error| format!("select-all list failed: {error:?}"))?;
    let element = page
        .elements
        .into_iter()
        .find(|entry| entry.snapshot.role.role == xenoteer_protocol::ElementRole::Entry)
        .ok_or("missing selection element")?
        .snapshot
        .element;
    let result = execute_semantic_action(
        &runtime.semantic_runtime(),
        Command::ElementSelection(ElementSelectionCommand {
            element,
            operation: ElementSelectionOperation::SelectAll,
            postcondition: None,
        }),
        tokio::time::Instant::now() + Duration::from_secs(2),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SemanticActionFailure::VerificationUnsupported)
    ));
    assert!(connector.semantic_calls()?.is_empty());
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(exit.mirror_stopped);
    Ok(())
}

#[tokio::test]
async fn reconnect_invalidates_partial_state_and_remints_the_generation()
-> Result<(), Box<dyn Error>> {
    let connector = FakeConnector::new(vec![Ok(vec![app(":1.20")?]), Ok(vec![app(":1.21")?])]);
    let runtime = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        DesktopId::new(),
        DesktopGeneration::new(),
        connector.clone(),
    )?;
    let reader = runtime.reader();
    wait_for_runtime(&reader, |snapshot| snapshot.mirror_ready).await?;
    assert_eq!(
        connector
            .latest_ingress()?
            .offer(BackendEvent::ConnectionClosed),
        EventOfferResult::Accepted
    );
    let reconnected = wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready && snapshot.accessibility_generation >= 2
    })
    .await?;
    assert!(reconnected.cache_revision >= 3);
    assert!(connector.connections.load(Ordering::SeqCst) >= 2);
    let exit = runtime.shutdown().await;
    assert_eq!(exit.actor_exit, Some(AtspiActorExit::Stopped));
    Ok(())
}

#[tokio::test]
async fn current_revision_public_event_overflow_is_never_treated_as_covered()
-> Result<(), Box<dyn Error>> {
    let mut config = configured(None)?;
    config.actor.event_capacity = 1;
    config.decoded_event_capacity = 1;
    config.actor.backend_event_capacity = 64;
    config.actor.request_capacity = 16;
    config.maintenance_interval = Duration::from_millis(250);
    let connector = FakeConnector::new(vec![
        Ok(vec![app(":1.30")?]),
        Ok(vec![app(":1.31")?]),
        Ok(vec![app(":1.32")?]),
        Ok(vec![app(":1.33")?]),
    ]);
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let runtime = spawn_accessibility_runtime_with_connector(
        config,
        desktop_id,
        desktop_generation,
        connector.clone(),
    )?;
    let reader = runtime.reader();
    let before = wait_for_runtime(&reader, |snapshot| snapshot.mirror_ready).await?;
    let ingress = connector.latest_ingress()?;
    for index in 0..2 {
        assert_eq!(
            ingress.offer(BackendEvent::ObjectChanged {
                source: None,
                kind: format!("metadata-{index}"),
            }),
            EventOfferResult::Accepted
        );
    }
    wait_for_runtime(&reader, |snapshot| {
        !snapshot.mirror_ready
            && snapshot.accessibility_generation == before.accessibility_generation
    })
    .await?;
    assert!(matches!(
        runtime
            .plane()
            .list_for(
                "test-principal",
                list_request(desktop_id, desktop_generation),
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    let rebuilt = wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready && snapshot.accessibility_generation > before.accessibility_generation
    })
    .await?;
    assert!(rebuilt.cache_revision > before.cache_revision);
    assert!(connector.connections.load(Ordering::SeqCst) >= 2);

    // The overflow epoch captured by the completed rebuild covers only the
    // actor's own delayed barrier. A later increment in the same generation
    // must fence the mirror and force a genuinely new actor generation.
    let rebuilt_ingress = connector.latest_ingress()?;
    for index in 0..2 {
        assert_eq!(
            rebuilt_ingress.offer(BackendEvent::ObjectChanged {
                source: None,
                kind: format!("post-fence-metadata-{index}"),
            }),
            EventOfferResult::Accepted
        );
    }
    wait_for_runtime(&reader, |snapshot| {
        !snapshot.mirror_ready
            && snapshot.accessibility_generation == rebuilt.accessibility_generation
    })
    .await?;
    assert!(matches!(
        runtime
            .plane()
            .list_for(
                "test-principal",
                list_request(desktop_id, desktop_generation),
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    let rebuilt_again = wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready
            && snapshot.accessibility_generation > rebuilt.accessibility_generation
    })
    .await?;
    assert!(rebuilt_again.cache_revision > rebuilt.cache_revision);
    assert!(connector.connections.load(Ordering::SeqCst) >= 3);
    let _ = runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cache_page_race_requests_rebuild_instead_of_publishing_partial_cache()
-> Result<(), Box<dyn Error>> {
    let mut config = configured(None)?;
    config.actor.read_page_nodes = 1;
    config.inter_page_delay = Duration::from_millis(100);
    config.maintenance_interval = Duration::from_millis(250);
    let connector = FakeConnector::new(vec![
        Ok(vec![app(":1.40")?, child(":1.40", "first")?]),
        Ok(vec![
            app(":1.41")?,
            child(":1.41", "first")?,
            child(":1.41", "second")?,
        ]),
    ]);
    let runtime = spawn_accessibility_runtime_with_connector(
        config,
        DesktopId::new(),
        DesktopGeneration::new(),
        connector.clone(),
    )?;
    let reader = runtime.reader();
    wait_for_runtime(&reader, |snapshot| {
        snapshot.actor_state == AtspiActorState::Healthy && !snapshot.mirror_ready
    })
    .await?;
    let ingress = connector.latest_ingress()?;
    tokio::time::sleep(Duration::from_millis(285)).await;
    assert_eq!(
        ingress.offer(BackendEvent::Cache(CacheEvent::Upsert(Box::new(child(
            ":1.40", "racing",
        )?)))),
        EventOfferResult::Accepted
    );
    let rebuilt = wait_for_runtime(&reader, |snapshot| {
        snapshot.mirror_ready && snapshot.accessibility_generation >= 2
    })
    .await?;
    assert!(rebuilt.cache_revision >= 4);
    assert!(connector.connections.load(Ordering::SeqCst) >= 2);
    let _ = runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn disabled_and_missing_backends_are_nonfatal_and_shutdown_is_bounded()
-> Result<(), Box<dyn Error>> {
    let disabled_connector = FakeConnector::new(Vec::new());
    let disabled = spawn_accessibility_runtime_with_connector(
        configured(Some("[accessibility]\nenabled = false"))?,
        DesktopId::new(),
        DesktopGeneration::new(),
        disabled_connector.clone(),
    )?;
    assert_eq!(
        disabled.reader().snapshot().actor_state,
        AtspiActorState::Disabled
    );
    assert_eq!(disabled_connector.connections.load(Ordering::SeqCst), 0);
    let disabled_exit = disabled.shutdown().await;
    assert_eq!(disabled_exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(disabled_exit.mirror_stopped);

    let missing_connector = FakeConnector::new(Vec::new());
    let missing = spawn_accessibility_runtime_with_connector(
        configured(None)?,
        DesktopId::new(),
        DesktopGeneration::new(),
        missing_connector.clone(),
    )?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let unavailable = missing.reader().snapshot();
    assert!(!unavailable.mirror_ready);
    assert!(matches!(
        unavailable.actor_state,
        AtspiActorState::Connecting | AtspiActorState::Reconnecting
    ));
    assert!(missing_connector.connections.load(Ordering::SeqCst) >= 1);
    let missing_exit = missing.shutdown().await;
    assert_eq!(missing_exit.actor_exit, Some(AtspiActorExit::Stopped));
    assert!(missing_exit.mirror_stopped);
    Ok(())
}
