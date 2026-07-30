#![allow(clippy::expect_used, clippy::panic)]

use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::task::yield_now;
use xenoteer_atspi::{
    CacheMutation, CacheMutationDetail, CacheMutationKind, CachePage, CachedNode,
    NormalizedCacheItem, ObjectAddress, SemanticObservationEvidence, SemanticValueEvidence,
    TextProtection,
};
use xenoteer_core::AccessibilityModelLimits;
use xenoteer_protocol::{
    AccessibilityQueryLimits, ElementListRequest, ElementOrder, ElementPredicate,
    ElementQueryRequest, ElementResolveRequest, ElementScope, ElementSelector,
    ElementSnapshotExpansion, ElementSnapshotRequest, ElementStringMatch, ElementWaitPredicate,
    ElementWaitQuantifier, ElementWaitRequest, ElementWaitStatus, ElementWaitTarget,
    WINDOW_IDENTITY_HASH_BYTES, WindowCorrelationEvidence, WindowCorrelationSignal,
    WindowIdentityHash, WindowRef,
};
use xenoteer_server::AccessibilityPlaneError;

use super::*;

const BUS: &str = ":1.42";
const APP_PATH: &str = "/org/example/App";

struct Fixture {
    plane: Arc<DaemonAccessibilityPlane>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

enum FakePollResponse {
    Immediate(Result<AccessibilityPollDispatch, AccessibilityPlaneError>),
    MirrorCommit {
        dispatch: AccessibilityPollDispatch,
        previous_revision: u64,
        mutation: CacheMutation,
    },
    Pending,
}

struct FakePollReconciler {
    responses: std::sync::Mutex<VecDeque<FakePollResponse>>,
    evidence: std::sync::Mutex<Vec<AccessibilityActionTargetEvidence>>,
    calls: std::sync::Mutex<Vec<tokio::time::Instant>>,
    plane: Weak<DaemonAccessibilityPlane>,
    plane_lock_was_free: AtomicBool,
}

impl FakePollReconciler {
    fn new(
        plane: &Arc<DaemonAccessibilityPlane>,
        responses: impl IntoIterator<Item = FakePollResponse>,
    ) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            evidence: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            plane: Arc::downgrade(plane),
            plane_lock_was_free: AtomicBool::new(true),
        }
    }
}

impl AccessibilityPollReconciler for FakePollReconciler {
    fn reconcile_exact<'a>(
        &'a self,
        evidence: AccessibilityActionTargetEvidence,
        _deadline: tokio::time::Instant,
    ) -> AccessibilityFuture<'a, Result<AccessibilityPollDispatch, AccessibilityPlaneError>> {
        if self
            .plane
            .upgrade()
            .is_some_and(|plane| plane.state.try_lock().is_err())
        {
            self.plane_lock_was_free.store(false, Ordering::SeqCst);
        }
        self.evidence.lock().expect("evidence lock").push(evidence);
        self.calls
            .lock()
            .expect("call lock")
            .push(tokio::time::Instant::now());
        let response = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("bounded fake poll response");
        let plane = self.plane.clone();
        Box::pin(async move {
            match response {
                FakePollResponse::Immediate(result) => result,
                FakePollResponse::MirrorCommit {
                    dispatch,
                    previous_revision,
                    mutation,
                } => {
                    plane
                        .upgrade()
                        .ok_or(AccessibilityPlaneError::Internal)?
                        .ingest_mutation(
                            dispatch.accessibility_generation,
                            previous_revision,
                            mutation,
                        )
                        .await?;
                    Ok(dispatch)
                }
                FakePollResponse::Pending => std::future::pending().await,
            }
        })
    }
}

impl Fixture {
    fn new(config: AccessibilityPlaneConfig) -> Self {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let plane = Arc::new(
            DaemonAccessibilityPlane::new(
                desktop_id,
                desktop_generation,
                AtspiGeneration::new(1).expect("generation"),
                config,
            )
            .expect("plane"),
        );
        Self {
            plane,
            desktop_id,
            desktop_generation,
        }
    }

    async fn bootstrap(&self, nodes: Vec<CachedNode>) {
        let event = self
            .plane
            .ingest_cache_page(cache_page(1, 1, None, nodes, None))
            .await
            .expect("bootstrap");
        assert_eq!(event.kind, AccessibilityIngestKind::Rebuilt);
        assert_eq!(event.atspi_generation.get(), 1);
    }

    fn list(&self, limit: u16) -> ElementListRequest {
        ElementListRequest {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            scope: ElementScope::Desktop,
            order: ElementOrder::ObjectPathAscending,
            limit,
            cursor: None,
            expansion: ElementSnapshotExpansion::default(),
            limits: AccessibilityQueryLimits::default(),
        }
    }

    fn selector(&self, predicates: Vec<ElementPredicate>) -> ElementSelector {
        ElementSelector {
            scope: ElementScope::Desktop,
            predicates,
            order: ElementOrder::ObjectPathAscending,
            result_index: None,
        }
    }

    fn wait(
        &self,
        after_revision: Option<AccessibilityRevision>,
        timeout_ms: u32,
    ) -> ElementWaitRequest {
        ElementWaitRequest {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            target: ElementWaitTarget::Selector {
                selector: self.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Ready".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            predicate: ElementWaitPredicate::Exists,
            after_revision,
            timeout_ms,
            allow_poll_fallback: false,
            expansion: ElementSnapshotExpansion::default(),
            limits: AccessibilityQueryLimits::default(),
        }
    }
}

fn address(path: &str) -> ObjectAddress {
    ObjectAddress::new(BUS, path).expect("address")
}

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

#[test]
fn atspi_state_words_project_known_bits_and_ignore_unknown_bits() {
    let words = state_words(&[8, 12, 24, 30, 63]);
    assert!(raw_state_contains(&words, 12));
    assert!(!raw_state_contains(&words, 11));
    let states = mapped_states(&words);
    assert!(states.contains(&ElementState::Enabled));
    assert!(states.contains(&ElementState::Focused));
    assert!(states.contains(&ElementState::Sensitive));
    assert!(states.contains(&ElementState::Visible));
    assert_eq!(states.len(), 4);
}

fn cached(path: &str, parent: Option<&str>, name: &str, role: u32, revision: u64) -> CachedNode {
    let item = NormalizedCacheItem {
        object: address(path),
        application: address(APP_PATH),
        parent: parent.map(address),
        index_in_parent: None,
        child_count: None,
        legacy_children: Vec::new(),
        interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
        name: name.to_owned(),
        description: String::new(),
        role,
        text_protection: if role == 40 {
            TextProtection::Protected
        } else {
            TextProtection::Unprotected
        },
        states: state_words(&[8, 24, 30]),
    };
    CachedNode {
        identity_fingerprint: item.identity_fingerprint(),
        live: xenoteer_atspi::CachedLiveMetadata::default(),
        item,
        application_generation: 1,
        revision,
    }
}

fn basic_nodes() -> Vec<CachedNode> {
    vec![
        cached(APP_PATH, None, "Application", 75, 1),
        cached("/org/example/App/Alpha", Some(APP_PATH), "Alpha", 43, 1),
        cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 1),
    ]
}

const WINDOW_PATH: &str = "/org/example/App/Main";
const BUTTON_PATH: &str = "/org/example/App/Main/Button";

fn correlation_nodes() -> Vec<CachedNode> {
    vec![
        cached(APP_PATH, None, "Application", 75, 1),
        cached(WINDOW_PATH, Some(APP_PATH), "Main", 23, 1),
        cached(BUTTON_PATH, Some(WINDOW_PATH), "Go", 43, 1),
    ]
}

fn fresh_correlation_observation(
    node: &CachedNode,
    top_level: Option<ObjectAddress>,
    read_epoch: u64,
) -> FreshAccessibilityCorrelationObservation {
    FreshAccessibilityCorrelationObservation {
        observation: SemanticObservationResult {
            accessibility_generation: 1,
            application_generation: node.application_generation,
            cache_revision: 1,
            read_epoch,
            object: node.item.object.clone(),
            application: node.item.application.clone(),
            evidence: SemanticObservationEvidence {
                identity_fingerprint: node.identity_fingerprint.clone(),
                parent: node.item.parent.clone(),
                index_in_parent: node.item.index_in_parent,
                role: node.item.role,
                states: node.item.states.clone(),
                interfaces: node.item.interfaces.clone(),
                bounds: Some(SemanticRect {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 80,
                }),
                top_level,
                application_pid: None,
                value: None,
                text: None,
                selected_children: None,
            },
        },
    }
}

fn correlation_window_snapshot(fixture: &Fixture, revision: u64) -> ObservationCorrelationSnapshot {
    let window = WindowRef {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("d".repeat(WINDOW_IDENTITY_HASH_BYTES))
            .expect("identity"),
    };
    let model_revision = WindowModelRevision::new(revision).expect("revision");
    let client_rect = xenoteer_protocol::WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(10, 20, 100, 80).expect("rect"),
    )
    .expect("window rect");
    ObservationCorrelationSnapshot {
        revision: model_revision,
        correlation_epoch: 1,
        observed_at: MonotonicMillis::new(revision),
        windows: vec![xenoteer_protocol::WindowSnapshot {
            xid_hex: window.xid_hex(),
            window,
            model_revision,
            metadata: xenoteer_protocol::WindowMetadata {
                title: Some(xenoteer_protocol::WindowText::new("Main", false).expect("title")),
                visible_title: None,
                icon_title: None,
                class: None,
                client_machine: None,
                window_types: Vec::new(),
                states: Vec::new(),
                allowed_actions: Vec::new(),
                protocols: Vec::new(),
            },
            process: xenoteer_protocol::WindowProcessCorrelation {
                reported_pid: None,
                managed_process: None,
                confidence: xenoteer_protocol::WindowProcessConfidence::None,
                evidence: Vec::new(),
                conflict: false,
            },
            state: xenoteer_protocol::WindowObservedState {
                map_state: xenoteer_protocol::WindowMapState::Viewable,
                minimized: false,
                hidden: false,
                urgent: false,
                modal: false,
                sticky: false,
                active: true,
                focused: true,
            },
            geometry: Some(xenoteer_protocol::WindowGeometry {
                client_rect,
                frame_rect: None,
                content_rect: client_rect,
                frame_extents: None,
            }),
            workspace: Some(0),
            client_leader: None,
            transient_for: None,
            group_leader: None,
            stacking_index: Some(0),
            has_accessibility_application: false,
            warnings: Vec::new(),
        }],
    }
}

struct FakeCorrelationObserver {
    responses: std::sync::Mutex<
        VecDeque<(
            ObjectAddress,
            Result<
                FreshAccessibilityCorrelationObservation,
                AccessibilityCorrelationCoordinatorError,
            >,
        )>,
    >,
}

impl FakeCorrelationObserver {
    fn new(
        responses: impl IntoIterator<
            Item = (
                ObjectAddress,
                Result<
                    FreshAccessibilityCorrelationObservation,
                    AccessibilityCorrelationCoordinatorError,
                >,
            ),
        >,
    ) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
        }
    }
}

impl AccessibilityCorrelationObserver for FakeCorrelationObserver {
    fn observe_exact<'a>(
        &'a self,
        evidence: &'a AccessibilityActionTargetEvidence,
        _deadline: tokio::time::Instant,
        _cancellation: CancellationToken,
    ) -> AccessibilityFuture<
        'a,
        Result<FreshAccessibilityCorrelationObservation, AccessibilityCorrelationCoordinatorError>,
    > {
        let (expected, response) = self
            .responses
            .lock()
            .expect("observer responses")
            .pop_front()
            .expect("bounded observer response");
        assert_eq!(&expected, evidence.object());
        Box::pin(async move { response })
    }
}

struct FakeCorrelationWindows {
    snapshots: std::sync::Mutex<
        VecDeque<Result<ObservationCorrelationSnapshot, AccessibilityCorrelationCoordinatorError>>,
    >,
    replace_results: std::sync::Mutex<
        VecDeque<Result<WindowModelRevision, AccessibilityCorrelationCoordinatorError>>,
    >,
    replacements: std::sync::Mutex<Vec<(WindowModelRevision, Vec<WindowRef>)>>,
}

impl FakeCorrelationWindows {
    fn new(
        snapshots: impl IntoIterator<
            Item = Result<ObservationCorrelationSnapshot, AccessibilityCorrelationCoordinatorError>,
        >,
        replace_results: impl IntoIterator<
            Item = Result<WindowModelRevision, AccessibilityCorrelationCoordinatorError>,
        >,
    ) -> Self {
        Self {
            snapshots: std::sync::Mutex::new(snapshots.into_iter().collect()),
            replace_results: std::sync::Mutex::new(replace_results.into_iter().collect()),
            replacements: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl AccessibilityCorrelationWindowSource for FakeCorrelationWindows {
    fn snapshot<'a>(
        &'a self,
    ) -> AccessibilityFuture<
        'a,
        Result<ObservationCorrelationSnapshot, AccessibilityCorrelationCoordinatorError>,
    > {
        let response = self
            .snapshots
            .lock()
            .expect("window snapshots")
            .pop_front()
            .expect("bounded window snapshot");
        Box::pin(async move { response })
    }

    fn replace<'a>(
        &'a self,
        expected_revision: WindowModelRevision,
        windows: Vec<WindowRef>,
    ) -> AccessibilityFuture<
        'a,
        Result<WindowModelRevision, AccessibilityCorrelationCoordinatorError>,
    > {
        self.replacements
            .lock()
            .expect("window replacements")
            .push((expected_revision, windows));
        let response = self
            .replace_results
            .lock()
            .expect("replace results")
            .pop_front()
            .unwrap_or(Ok(expected_revision));
        Box::pin(async move { response })
    }
}

#[test]
fn correlation_configuration_and_managed_pid_promotion_are_bounded_and_broker_anchored() {
    let invalid = AccessibilityCorrelationCoordinatorConfig {
        interval: Duration::ZERO,
        ..AccessibilityCorrelationCoordinatorConfig::default()
    };
    assert!(matches!(
        invalid.validate(),
        Err(AccessibilityCorrelationCoordinatorError::InvalidConfiguration)
    ));

    let node = cached(APP_PATH, None, "Application", 75, 1);
    let fresh = SemanticObservationResult {
        accessibility_generation: 1,
        application_generation: 1,
        cache_revision: 1,
        read_epoch: 1,
        object: address(APP_PATH),
        application: address(APP_PATH),
        evidence: SemanticObservationEvidence {
            identity_fingerprint: node.identity_fingerprint,
            parent: None,
            index_in_parent: None,
            role: 75,
            states: state_words(&[8, 24, 30]),
            interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
            bounds: None,
            top_level: None,
            application_pid: Some(4_242),
            value: None,
            text: None,
            selected_children: None,
        },
    };
    let window = WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("a".repeat(WINDOW_IDENTITY_HASH_BYTES))
            .expect("identity"),
    };
    let candidate = AccessibilityWindowCandidate {
        window,
        live: true,
        process_id: Some(4_242),
        managed_process_id: None,
        top_level_extents: None,
        title: None,
        application_identity: None,
        toolkit_identity: None,
        focused: false,
        focus_changed_at: None,
        created_at: None,
        observed_at: MonotonicMillis::new(1),
        client_leader: None,
    };
    assert_eq!(
        broker_verified_managed_pid(&fresh, std::slice::from_ref(&candidate)),
        None
    );
    let mut broker_anchored = candidate;
    broker_anchored.managed_process_id = Some(4_242);
    assert_eq!(
        broker_verified_managed_pid(&fresh, &[broker_anchored]),
        Some(4_242)
    );
}

#[tokio::test]
async fn coordinator_background_pass_installs_complete_weak_and_none_evidence() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let nodes = correlation_nodes();
    fixture.bootstrap(nodes.clone()).await;
    let observer = Arc::new(FakeCorrelationObserver::new([
        (
            address(APP_PATH),
            Err(AccessibilityCorrelationCoordinatorError::Actor(
                SemanticError::InterfaceUnavailable("custom application"),
            )),
        ),
        (
            address(WINDOW_PATH),
            Ok(fresh_correlation_observation(
                &nodes[1],
                Some(address(WINDOW_PATH)),
                2,
            )),
        ),
    ]));
    let snapshot = correlation_window_snapshot(&fixture, 1);
    let expected_window = snapshot.windows[0].window.clone();
    let windows = Arc::new(FakeCorrelationWindows::new([Ok(snapshot)], []));
    let coordinator = AccessibilityCorrelationCoordinator::new(
        Arc::clone(&fixture.plane),
        observer,
        windows.clone(),
        AccessibilityCorrelationCoordinatorConfig::default(),
    )
    .expect("coordinator");

    let pass = coordinator
        .reconcile_once(CancellationToken::new())
        .await
        .expect("background pass");
    assert_eq!((pass.target_count, pass.correlated_window_count), (2, 1));
    let state = fixture.plane.state.lock().await;
    let application = state
        .correlations
        .get(&address(APP_PATH))
        .expect("application none evidence");
    assert_eq!(
        application.correlation.confidence,
        WindowCorrelationConfidence::None
    );
    assert!(application.correlation.window.is_none());
    let top_level = state
        .correlations
        .get(&address(WINDOW_PATH))
        .expect("top-level weak evidence");
    assert_eq!(
        top_level.correlation.confidence,
        WindowCorrelationConfidence::Weak
    );
    assert_eq!(
        top_level.correlation.window.as_ref(),
        Some(&expected_window)
    );
    drop(state);
    let replacements = windows.replacements.lock().expect("replacements");
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].1, vec![expected_window]);
}

#[tokio::test]
async fn coordinator_optional_explicit_signal_child_top_level_and_fresh_recheck_are_exact() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = correlation_nodes();
    nodes[1] = cached(WINDOW_PATH, Some(APP_PATH), "Main", 39, 1);
    fixture.bootstrap(nodes.clone()).await;
    let button = element_by_name(&fixture, "Go").await;
    let snapshots = [
        correlation_window_snapshot(&fixture, 2),
        correlation_window_snapshot(&fixture, 3),
    ];
    let explicit_window = snapshots[1].windows[0].window.clone();
    let observer = Arc::new(FakeCorrelationObserver::new([
        (
            address(BUTTON_PATH),
            Ok(fresh_correlation_observation(
                &nodes[2],
                Some(address(WINDOW_PATH)),
                1,
            )),
        ),
        (
            address(WINDOW_PATH),
            Ok(fresh_correlation_observation(
                &nodes[1],
                Some(address(WINDOW_PATH)),
                2,
            )),
        ),
        (
            address(BUTTON_PATH),
            Ok(fresh_correlation_observation(
                &nodes[2],
                Some(address(WINDOW_PATH)),
                3,
            )),
        ),
        (
            address(WINDOW_PATH),
            Ok(fresh_correlation_observation(
                &nodes[1],
                Some(address(WINDOW_PATH)),
                4,
            )),
        ),
    ]));
    let windows = Arc::new(FakeCorrelationWindows::new(
        snapshots.into_iter().map(Ok),
        [],
    ));
    let coordinator = AccessibilityCorrelationCoordinator::new(
        Arc::clone(&fixture.plane),
        observer,
        windows,
        AccessibilityCorrelationCoordinatorConfig::default(),
    )
    .expect("coordinator");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

    let derived = coordinator
        .correlate_element(&button, None, deadline, CancellationToken::new())
        .await
        .expect("derived correlation");
    assert!(
        derived.correlation().evidence.iter().all(|evidence| {
            evidence.signal != WindowCorrelationSignal::ExplicitCallerReference
        })
    );
    let explicit = coordinator
        .correlate_element(
            &button,
            Some(explicit_window.clone()),
            deadline,
            CancellationToken::new(),
        )
        .await
        .expect("explicit correlation");
    assert!(explicit.correlation().evidence.iter().any(|evidence| {
        evidence.signal == WindowCorrelationSignal::ExplicitCallerReference && evidence.matched
    }));
    assert_eq!(
        explicit.admission_element_observation().object,
        address(BUTTON_PATH)
    );
    assert_eq!(
        explicit.admission_correlation_observation().object,
        address(WINDOW_PATH)
    );
    assert_eq!(explicit.admission_window_snapshot().revision.get(), 3);

    let queue_child =
        fresh_correlation_observation(&nodes[2], Some(address(WINDOW_PATH)), 5).observation;
    let queue_top =
        fresh_correlation_observation(&nodes[1], Some(address(WINDOW_PATH)), 6).observation;
    let refreshed = explicit
        .with_fresh_observations(
            queue_child,
            queue_top,
            correlation_window_snapshot(&fixture, 4),
            AccessibilityCorrelationLimits::default(),
        )
        .expect("fresh correlation");
    let raw = SemanticRect {
        x: 10,
        y: 20,
        width: 100,
        height: 80,
    };
    let profiled = AccessibilityProfiledRect {
        atspi_screen: raw,
        root_physical: Rect::new(10, 20, 100, 80).expect("profiled rect"),
    };
    let click = refreshed
        .click_observation(
            profiled,
            Some(profiled),
            Rect::new(0, 0, 1_024, 768).expect("root bounds"),
            AccessibilityCorrelationLimits::default(),
        )
        .expect("click observation");
    assert_eq!(click.element, button);
    assert_eq!(click.read_epoch, 5);
    assert_eq!(click.correlation.window.as_ref(), Some(&explicit_window));

    let regression = explicit.with_fresh_observations(
        fresh_correlation_observation(&nodes[2], Some(address(WINDOW_PATH)), 7).observation,
        fresh_correlation_observation(&nodes[1], Some(address(WINDOW_PATH)), 8).observation,
        correlation_window_snapshot(&fixture, 1),
        AccessibilityCorrelationLimits::default(),
    );
    assert!(matches!(
        regression,
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    ));

    let queue_child =
        fresh_correlation_observation(&nodes[2], Some(address(WINDOW_PATH)), 9).observation;
    let mut later_queue_top =
        fresh_correlation_observation(&nodes[1], Some(address(WINDOW_PATH)), 10).observation;
    later_queue_top.cache_revision = 2;
    let mixed_actor_universe = explicit.with_fresh_observations(
        queue_child,
        later_queue_top,
        correlation_window_snapshot(&fixture, 4),
        AccessibilityCorrelationLimits::default(),
    );
    assert!(matches!(
        mixed_actor_universe,
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    ));

    let non_serial_pair = explicit.with_fresh_observations(
        fresh_correlation_observation(&nodes[2], Some(address(WINDOW_PATH)), 11).observation,
        fresh_correlation_observation(&nodes[1], Some(address(WINDOW_PATH)), 11).observation,
        correlation_window_snapshot(&fixture, 4),
        AccessibilityCorrelationLimits::default(),
    );
    assert!(matches!(
        non_serial_pair,
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    ));

    let guard = fixture.plane.state.lock().await;
    assert_eq!(
        fixture
            .plane
            .revalidate_explicit_correlation_evidence_blocking(&explicit),
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    drop(guard);
    fixture
        .plane
        .revalidate_explicit_correlation_evidence_blocking(&explicit)
        .expect("exact fence still current");

    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Unrelated",
                    Some(APP_PATH),
                    "Unrelated",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("unrelated cache mutation");
    fixture
        .plane
        .revalidate_explicit_correlation_evidence_blocking(&explicit)
        .expect("unrelated global revision preserves exact target births");

    fixture
        .plane
        .ingest_mutation(
            1,
            2,
            CacheMutation {
                revision: 3,
                kind: CacheMutationKind::Refreshed,
                detail: CacheMutationDetail::Refreshed(Box::new(cached(
                    BUTTON_PATH,
                    Some(WINDOW_PATH),
                    "Go",
                    43,
                    3,
                ))),
            },
        )
        .await
        .expect("exact target refresh");
    assert!(matches!(
        fixture
            .plane
            .revalidate_explicit_correlation_evidence_blocking(&explicit),
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
}

#[tokio::test]
async fn coordinator_retries_only_stale_evidence_and_exhausts_boundedly() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let nodes = correlation_nodes();
    fixture.bootstrap(nodes.clone()).await;
    let button = element_by_name(&fixture, "Go").await;
    let stale = || {
        AccessibilityCorrelationCoordinatorError::Actor(SemanticError::StaleCacheRevision {
            expected: 1,
            current: 2,
        })
    };
    let observer = Arc::new(FakeCorrelationObserver::new([
        (address(BUTTON_PATH), Err(stale())),
        (
            address(BUTTON_PATH),
            Ok(fresh_correlation_observation(
                &nodes[2],
                Some(address(WINDOW_PATH)),
                1,
            )),
        ),
        (
            address(WINDOW_PATH),
            Ok(fresh_correlation_observation(
                &nodes[1],
                Some(address(WINDOW_PATH)),
                2,
            )),
        ),
    ]));
    let windows = Arc::new(FakeCorrelationWindows::new(
        [Ok(correlation_window_snapshot(&fixture, 1))],
        [],
    ));
    let coordinator = AccessibilityCorrelationCoordinator::new(
        Arc::clone(&fixture.plane),
        observer,
        windows,
        AccessibilityCorrelationCoordinatorConfig::default(),
    )
    .expect("coordinator");
    coordinator
        .correlate_element(
            &button,
            None,
            tokio::time::Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .expect("single stale retry");

    let exhausted_observer = Arc::new(FakeCorrelationObserver::new([
        (address(BUTTON_PATH), Err(stale())),
        (address(BUTTON_PATH), Err(stale())),
    ]));
    let exhausted = AccessibilityCorrelationCoordinator::new(
        Arc::clone(&fixture.plane),
        exhausted_observer,
        Arc::new(FakeCorrelationWindows::new([], [])),
        AccessibilityCorrelationCoordinatorConfig::default(),
    )
    .expect("coordinator")
    .correlate_element(
        &button,
        None,
        tokio::time::Instant::now() + Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        exhausted,
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    ));
}

#[tokio::test]
async fn coordinator_clears_both_planes_when_window_commit_fails() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let nodes = correlation_nodes();
    fixture.bootstrap(nodes.clone()).await;
    let observer = Arc::new(FakeCorrelationObserver::new([
        (
            address(APP_PATH),
            Ok(fresh_correlation_observation(&nodes[0], None, 1)),
        ),
        (
            address(WINDOW_PATH),
            Ok(fresh_correlation_observation(
                &nodes[1],
                Some(address(WINDOW_PATH)),
                2,
            )),
        ),
    ]));
    let windows = Arc::new(FakeCorrelationWindows::new(
        [
            Ok(correlation_window_snapshot(&fixture, 1)),
            Ok(correlation_window_snapshot(&fixture, 2)),
        ],
        [
            Err(AccessibilityCorrelationCoordinatorError::Window(
                ControlPlaneError::StaleReference {
                    current_generation: Some(fixture.desktop_generation),
                },
            )),
            Ok(WindowModelRevision::new(2).expect("revision")),
        ],
    ));
    let coordinator = AccessibilityCorrelationCoordinator::new(
        Arc::clone(&fixture.plane),
        observer,
        windows.clone(),
        AccessibilityCorrelationCoordinatorConfig {
            max_stale_retries: 0,
            ..AccessibilityCorrelationCoordinatorConfig::default()
        },
    )
    .expect("coordinator");
    assert!(matches!(
        coordinator.reconcile_once(CancellationToken::new()).await,
        Err(AccessibilityCorrelationCoordinatorError::StaleEvidenceExhausted)
    ));
    assert!(fixture.plane.state.lock().await.correlations.is_empty());
    let replacements = windows.replacements.lock().expect("replacements");
    assert_eq!(replacements.len(), 2);
    assert_eq!(replacements[0].1.len(), 1);
    assert!(replacements[1].1.is_empty());
}

#[tokio::test]
async fn normalized_snapshots_project_content_free_text_metadata_for_known_protected_nodes() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let ready = nodes
        .iter_mut()
        .find(|node| node.item.name == "Ready")
        .expect("ready node");
    ready.live.bounds = Some(xenoteer_atspi::SemanticRect {
        x: 11,
        y: 22,
        width: 33,
        height: 44,
    });
    ready.live.value = Some(xenoteer_atspi::SemanticValueEvidence {
        current: 2.0,
        minimum: 1.0,
        maximum: 3.0,
        minimum_increment: 0.5,
    });
    ready.live.text = Some(xenoteer_atspi::CachedTextMetadata {
        character_count: Some(9),
        caret_offset: Some(4),
        selections: vec![xenoteer_atspi::SelectionRangeEvidence { start: 1, end: 3 }],
    });
    let mut password = cached(
        "/org/example/App/Password",
        Some(APP_PATH),
        "Password",
        40,
        1,
    );
    password.live.text = Some(xenoteer_atspi::CachedTextMetadata {
        character_count: Some(12),
        caret_offset: Some(6),
        selections: vec![xenoteer_atspi::SelectionRangeEvidence { start: 0, end: 12 }],
    });
    nodes.push(password);
    nodes.sort_by(|left, right| left.item.object.cmp(&right.item.object));
    fixture.bootstrap(nodes).await;

    let mut request = fixture.list(10);
    request.expansion.value = true;
    request.expansion.text_metadata = true;
    let page = fixture
        .plane
        .list_for("alice", request)
        .await
        .expect("list");
    let ready = page
        .elements
        .iter()
        .find(|entry| entry.snapshot.name.as_deref() == Some("Ready"))
        .expect("ready snapshot");
    assert_eq!(
        ready
            .snapshot
            .component
            .as_ref()
            .and_then(|component| component.extents),
        Some(Rect::new(11, 22, 33, 44).expect("rect"))
    );
    assert_eq!(
        ready
            .snapshot
            .component
            .as_ref()
            .map(|component| component.coordinate_space),
        Some(CoordinateSpace::AtspiScreen)
    );
    let value = ready.snapshot.value.as_ref().expect("value");
    assert_eq!(
        (value.current, value.minimum, value.maximum, value.increment),
        (2.0, Some(1.0), Some(3.0), Some(0.5))
    );
    let text = ready.snapshot.text.as_ref().expect("text");
    assert_eq!((text.character_count, text.caret_offset), (9, 4));
    assert_eq!(text.selections, vec![ElementTextRange { start: 1, end: 3 }]);
    assert!(text.content.is_none());

    let password = page
        .elements
        .iter()
        .find(|entry| {
            entry
                .snapshot
                .element
                .object_path
                .as_str()
                .ends_with("/Password")
        })
        .expect("password snapshot");
    assert!(password.snapshot.is_protected());
    assert!(password.snapshot.name.is_none());
    assert!(password.snapshot.description.is_none());
    let text = password.snapshot.text.as_ref().expect("safe text metadata");
    assert_eq!((text.character_count, text.caret_offset), (12, 6));
    assert_eq!(
        text.selections,
        vec![ElementTextRange { start: 0, end: 12 }]
    );
    assert!(text.content.is_none());
    assert!(text.protected);
}

#[tokio::test]
async fn reserved_metadata_contract_fails_as_invalid_before_plane_evaluation() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let target = element_by_name(&fixture, "Ready").await;
    let matcher = || ElementStringMatch::Exact {
        value: "x".to_owned(),
        case_sensitive: true,
    };
    let unsupported = vec![
        ElementPredicate::AccessibleId { matcher: matcher() },
        ElementPredicate::Attribute {
            name: "data-id".to_owned(),
            matcher: matcher(),
        },
        ElementPredicate::Action { matcher: matcher() },
        ElementPredicate::Relation {
            relation: xenoteer_protocol::ElementRelationType::LabelFor,
            target,
        },
    ];
    for predicate in unsupported {
        let request = ElementQueryRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            selector: fixture.selector(vec![predicate]),
            limit: 10,
            cursor: None,
            expansion: ElementSnapshotExpansion::default(),
            limits: AccessibilityQueryLimits::default(),
        };
        assert_eq!(
            fixture.plane.query_for("alice", request).await,
            Err(AccessibilityPlaneError::InvalidRequest)
        );
    }

    for expansion in [
        ElementSnapshotExpansion {
            actions: true,
            ..ElementSnapshotExpansion::default()
        },
        ElementSnapshotExpansion {
            attributes: true,
            ..ElementSnapshotExpansion::default()
        },
        ElementSnapshotExpansion {
            relations: true,
            ..ElementSnapshotExpansion::default()
        },
        ElementSnapshotExpansion {
            text_metadata: true,
            text_content: true,
            ..ElementSnapshotExpansion::default()
        },
    ] {
        let mut request = fixture.list(10);
        request.expansion = expansion;
        assert_eq!(
            fixture.plane.list_for("alice", request).await,
            Err(AccessibilityPlaneError::InvalidRequest)
        );
    }

    let mut text_wait = fixture.wait(None, 10);
    text_wait.predicate = ElementWaitPredicate::Text { matcher: matcher() };
    assert_eq!(
        fixture.plane.wait_for(text_wait).await,
        Err(AccessibilityPlaneError::InvalidRequest)
    );
    let mut geometry_wait = fixture.wait(None, 10);
    geometry_wait.predicate = ElementWaitPredicate::Geometry {
        coordinate_space: CoordinateSpace::RootPhysical,
        intersects: Rect::new(0, 0, 10, 10).expect("rect"),
    };
    assert_eq!(
        fixture.plane.wait_for(geometry_wait).await,
        Err(AccessibilityPlaneError::InvalidRequest)
    );
}

#[tokio::test]
async fn component_selector_and_geometry_wait_use_complete_atspi_screen_evidence() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let alpha = nodes
        .iter_mut()
        .find(|node| node.item.name == "Alpha")
        .expect("alpha");
    alpha
        .item
        .interfaces
        .push("org.a11y.atspi.Component".to_owned());
    alpha.live.bounds = Some(SemanticRect {
        x: 20,
        y: 30,
        width: 40,
        height: 50,
    });
    fixture.bootstrap(nodes).await;

    let query = ElementQueryRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        selector: fixture.selector(vec![ElementPredicate::ComponentIntersects {
            coordinate_space: CoordinateSpace::AtspiScreen,
            rect: Rect::new(25, 35, 5, 5).expect("rect"),
        }]),
        limit: 10,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    let page = fixture
        .plane
        .query_for("alice", query)
        .await
        .expect("component query");
    assert_eq!(page.elements.len(), 1);
    assert_eq!(page.elements[0].snapshot.name.as_deref(), Some("Alpha"));
    assert_eq!(
        page.elements[0]
            .snapshot
            .component
            .as_ref()
            .map(|component| component.coordinate_space),
        Some(CoordinateSpace::AtspiScreen)
    );

    let alpha = page.elements[0].snapshot.element.clone();
    let mut wait = fixture.wait(None, 20);
    wait.target = ElementWaitTarget::Reference { element: alpha };
    wait.predicate = ElementWaitPredicate::Geometry {
        coordinate_space: CoordinateSpace::AtspiScreen,
        intersects: Rect::new(55, 75, 10, 10).expect("rect"),
    };
    let result = fixture.plane.wait_for(wait).await.expect("geometry wait");
    assert_eq!(result.status, ElementWaitStatus::Matched);
    assert!(result.predicate_satisfied);
    assert_eq!(result.matched_count, 1);
}

#[tokio::test]
async fn component_predicate_rejects_incomplete_live_bounds_without_false_negative() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    nodes[1]
        .item
        .interfaces
        .push("org.a11y.atspi.Component".to_owned());
    fixture.bootstrap(nodes).await;
    let request = ElementQueryRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        selector: fixture.selector(vec![ElementPredicate::ComponentIntersects {
            coordinate_space: CoordinateSpace::AtspiScreen,
            rect: Rect::new(0, 0, 10, 10).expect("rect"),
        }]),
        limit: 10,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    assert_eq!(
        fixture.plane.query_for("alice", request).await,
        Err(AccessibilityPlaneError::CapabilityUnavailable)
    );
}

#[tokio::test]
async fn advertised_live_interfaces_require_evidence_but_password_redaction_is_not_missing_data() {
    for (interface, expansion) in [
        (
            "org.a11y.atspi.Value",
            ElementSnapshotExpansion {
                value: true,
                component: false,
                ..ElementSnapshotExpansion::default()
            },
        ),
        (
            "org.a11y.atspi.Text",
            ElementSnapshotExpansion {
                text_metadata: true,
                component: false,
                ..ElementSnapshotExpansion::default()
            },
        ),
        (
            "org.a11y.atspi.Component",
            ElementSnapshotExpansion::default(),
        ),
    ] {
        let fixture = Fixture::new(AccessibilityPlaneConfig::default());
        let mut nodes = basic_nodes();
        nodes[1].item.interfaces.push(interface.to_owned());
        fixture.bootstrap(nodes).await;
        let mut request = fixture.list(10);
        request.expansion = expansion;
        assert_eq!(
            fixture.plane.list_for("alice", request).await,
            Err(AccessibilityPlaneError::CapabilityUnavailable)
        );
        if interface.ends_with(".Value") {
            let query = ElementQueryRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                selector: fixture.selector(vec![ElementPredicate::ValueRange {
                    minimum: Some(1.0),
                    maximum: None,
                }]),
                limit: 10,
                cursor: None,
                expansion: ElementSnapshotExpansion::default(),
                limits: AccessibilityQueryLimits::default(),
            };
            assert_eq!(
                fixture.plane.query_for("alice", query).await,
                Err(AccessibilityPlaneError::CapabilityUnavailable)
            );
            let mut wait = fixture.wait(None, 10);
            wait.target = ElementWaitTarget::Reference {
                element: element_by_name(&fixture, "Alpha").await,
            };
            wait.predicate = ElementWaitPredicate::Value {
                minimum: Some(1.0),
                maximum: None,
            };
            assert_eq!(
                fixture.plane.wait_for(wait).await,
                Err(AccessibilityPlaneError::CapabilityUnavailable)
            );
        }
    }

    let protected = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let mut password = cached(
        "/org/example/App/Password",
        Some(APP_PATH),
        "Password",
        40,
        1,
    );
    password.item.interfaces.extend([
        "org.a11y.atspi.Text".to_owned(),
        "org.a11y.atspi.Value".to_owned(),
    ]);
    nodes.push(password);
    nodes.sort_by(|left, right| left.item.object.cmp(&right.item.object));
    protected.bootstrap(nodes).await;
    let mut request = protected.list(10);
    request.expansion = ElementSnapshotExpansion {
        value: true,
        text_metadata: true,
        component: false,
        ..ElementSnapshotExpansion::default()
    };
    let page = protected
        .plane
        .list_for("alice", request)
        .await
        .expect("protected redaction is intentional");
    let password = page
        .elements
        .iter()
        .find(|entry| entry.snapshot.role.role == ElementRole::PasswordText)
        .expect("password");
    assert!(password.snapshot.is_protected());
    assert!(password.snapshot.value.is_none());
    assert!(password.snapshot.text.is_none());
}

fn estimated_nodes_bytes(nodes: &[CachedNode]) -> usize {
    nodes
        .iter()
        .map(|node| checked_raw_node_bytes(node).expect("bounded test node"))
        .try_fold(0_usize, |total, bytes| total.checked_add(bytes))
        .expect("bounded test page")
}

fn cache_page(
    accessibility_generation: u64,
    revision: u64,
    after: Option<ObjectAddress>,
    nodes: Vec<CachedNode>,
    next_after: Option<ObjectAddress>,
) -> CachePage {
    let estimated_bytes = estimated_nodes_bytes(&nodes);
    CachePage {
        accessibility_generation,
        revision,
        event_overflow_epoch: 0,
        after,
        nodes,
        next_after,
        estimated_bytes,
    }
}

async fn element_by_name(fixture: &Fixture, name: &str) -> ElementRef {
    let result = fixture
        .plane
        .resolve_for(ElementResolveRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            selector: fixture.selector(vec![ElementPredicate::Name {
                matcher: ElementStringMatch::Exact {
                    value: name.to_owned(),
                    case_sensitive: true,
                },
            }]),
            expansion: ElementSnapshotExpansion::default(),
            limits: AccessibilityQueryLimits::default(),
        })
        .await
        .expect("resolve");
    assert_eq!(result.element.snapshot.revision, result.snapshot_revision);
    result.element.snapshot.element
}

#[tokio::test]
async fn cursor_is_principal_bound_one_use_and_exactly_bound() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let first = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("first page");
    let cursor = first.next_cursor.expect("cursor");

    let mut next = fixture.list(1);
    next.cursor = Some(cursor.clone());
    assert_eq!(
        fixture.plane.list_for("mallory", next.clone()).await,
        Err(AccessibilityPlaneError::PermissionDenied)
    );
    let second = fixture
        .plane
        .list_for("alice", next.clone())
        .await
        .expect("owner continuation");
    assert_eq!(second.elements.len(), 1);
    assert!(matches!(
        fixture.plane.list_for("alice", next).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));

    let cursor = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("new page")
        .next_cursor
        .expect("cursor");
    let mut rebound = fixture.list(2);
    rebound.cursor = Some(cursor);
    assert_eq!(
        fixture.plane.list_for("alice", rebound).await,
        Err(AccessibilityPlaneError::InvalidRequest)
    );
}

#[tokio::test]
async fn operator_limits_clamp_queries_pages_and_cursor_continuations() {
    let fixture = Fixture::new(AccessibilityPlaneConfig {
        max_nodes_per_query: 3,
        max_selector_depth: 1,
        max_query_matches: 3,
        query_timeout_ms: 1_000,
        max_snapshot_nodes: 1,
        ..AccessibilityPlaneConfig::default()
    });
    fixture.bootstrap(basic_nodes()).await;

    let effective = fixture
        .plane
        .effective_query_limits(AccessibilityQueryLimits::default())
        .expect("effective limits");
    assert_eq!(
        effective,
        AccessibilityQueryLimits {
            max_visited_nodes: 3,
            max_depth: 1,
            max_matches: 3,
            timeout_ms: 1_000,
        }
    );
    assert_eq!(fixture.plane.effective_page_limit(100, effective), 1);

    let first = fixture
        .plane
        .list_for("alice", fixture.list(100))
        .await
        .expect("clamped list page");
    assert_eq!(first.elements.len(), 1);
    let mut continuation = fixture.list(100);
    continuation.cursor = first.next_cursor;
    let second = fixture
        .plane
        .list_for("alice", continuation)
        .await
        .expect("same clamped list continuation");
    assert_eq!(second.elements.len(), 1);

    let query = ElementQueryRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        selector: fixture.selector(Vec::new()),
        limit: 100,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    let first = fixture
        .plane
        .query_for("alice", query.clone())
        .await
        .expect("clamped query page");
    assert_eq!(first.elements.len(), 1);
    let mut continuation = query;
    continuation.cursor = first.next_cursor;
    let second = fixture
        .plane
        .query_for("alice", continuation)
        .await
        .expect("same clamped query continuation");
    assert_eq!(second.elements.len(), 1);
}

#[tokio::test]
async fn operator_traversal_and_encoded_snapshot_ceilings_fail_closed() {
    let traversal_limited = Fixture::new(AccessibilityPlaneConfig {
        max_nodes_per_query: 2,
        max_snapshot_nodes: 2,
        ..AccessibilityPlaneConfig::default()
    });
    traversal_limited.bootstrap(basic_nodes()).await;
    assert_eq!(
        traversal_limited
            .plane
            .list_for("alice", traversal_limited.list(10))
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
    assert_eq!(
        traversal_limited
            .plane
            .resolve_for(ElementResolveRequest {
                desktop_id: traversal_limited.desktop_id,
                desktop_generation: traversal_limited.desktop_generation,
                selector: traversal_limited.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Ready".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                expansion: ElementSnapshotExpansion::default(),
                limits: AccessibilityQueryLimits::default(),
            })
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
    assert_eq!(
        traversal_limited
            .plane
            .wait_for(traversal_limited.wait(None, 10))
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );

    let encoded_limited = Fixture::new(AccessibilityPlaneConfig {
        max_snapshot_bytes: 1,
        ..AccessibilityPlaneConfig::default()
    });
    encoded_limited.bootstrap(basic_nodes()).await;
    let element = {
        let state = encoded_limited.plane.state.lock().await;
        state
            .elements
            .get(&address("/org/example/App/Ready"))
            .cloned()
            .expect("ready element")
    };
    assert_eq!(
        encoded_limited
            .plane
            .list_for("alice", encoded_limited.list(1))
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
    assert!(
        encoded_limited.plane.state.lock().await.cursors.is_empty(),
        "a rejected page must not retain its inaccessible cursor"
    );
    assert_eq!(
        encoded_limited
            .plane
            .resolve_for(ElementResolveRequest {
                desktop_id: encoded_limited.desktop_id,
                desktop_generation: encoded_limited.desktop_generation,
                selector: encoded_limited.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Ready".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                expansion: ElementSnapshotExpansion::default(),
                limits: AccessibilityQueryLimits::default(),
            })
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
    assert_eq!(
        encoded_limited
            .plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: encoded_limited.desktop_id,
                desktop_generation: encoded_limited.desktop_generation,
                element,
                expansion: ElementSnapshotExpansion::default(),
            })
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
    assert_eq!(
        encoded_limited
            .plane
            .wait_for(encoded_limited.wait(None, 10))
            .await,
        Err(AccessibilityPlaneError::QueryLimitExceeded)
    );
}

#[tokio::test]
async fn cursor_expires_and_mutation_invalidates_revision() {
    let config = AccessibilityPlaneConfig {
        cursor_ttl: Duration::from_millis(5),
        ..AccessibilityPlaneConfig::default()
    };
    let fixture = Fixture::new(config);
    fixture.bootstrap(basic_nodes()).await;
    let cursor = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("page")
        .next_cursor
        .expect("cursor");
    tokio::time::sleep(Duration::from_millis(15)).await;
    let mut expired = fixture.list(1);
    expired.cursor = Some(cursor);
    assert!(matches!(
        fixture.plane.list_for("alice", expired).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));

    let cursor = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("page")
        .next_cursor
        .expect("cursor");
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready again",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("mutation");
    let mut stale = fixture.list(1);
    stale.cursor = Some(cursor);
    assert!(matches!(
        fixture.plane.list_for("alice", stale).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
}

#[tokio::test]
async fn cursor_cap_is_enforced_per_principal() {
    assert!(matches!(
        DaemonAccessibilityPlane::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            AtspiGeneration::new(1).expect("generation"),
            AccessibilityPlaneConfig {
                cursor_ttl: Duration::from_millis(u64::from(ACCESSIBILITY_CURSOR_TTL_MS) + 1),
                ..AccessibilityPlaneConfig::default()
            },
        ),
        Err(AccessibilityPlaneError::InvalidRequest)
    ));
    let config = AccessibilityPlaneConfig {
        max_total_cursors: 2,
        max_cursors_per_principal: 1,
        ..AccessibilityPlaneConfig::default()
    };
    let fixture = Fixture::new(config);
    fixture.bootstrap(basic_nodes()).await;
    fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("first cursor");
    assert_eq!(
        fixture.plane.list_for("alice", fixture.list(1)).await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    fixture
        .plane
        .list_for("bob", fixture.list(1))
        .await
        .expect("independent principal capacity");
}

#[tokio::test]
async fn exact_resolution_reports_ambiguity_and_reference_fences() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    assert_eq!(
        fixture
            .plane
            .resolve_for(ElementResolveRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                selector: fixture.selector(Vec::new()),
                expansion: ElementSnapshotExpansion::default(),
                limits: AccessibilityQueryLimits::default(),
            })
            .await,
        Err(AccessibilityPlaneError::AmbiguousTarget)
    );

    let old = element_by_name(&fixture, "Ready").await;
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Removed,
                detail: CacheMutationDetail::Removed(vec![address("/org/example/App/Ready")]),
            },
        )
        .await
        .expect("remove");
    fixture
        .plane
        .ingest_mutation(
            1,
            2,
            CacheMutation {
                revision: 3,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    3,
                ))),
            },
        )
        .await
        .expect("successor");
    let old_request = ElementSnapshotRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        element: old.clone(),
        expansion: ElementSnapshotExpansion::default(),
    };
    assert!(matches!(
        fixture.plane.snapshot_for(old_request).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));

    let mut absent_path = element_by_name(&fixture, "Ready").await;
    absent_path.object_path = AtspiObjectPath::new("/org/example/App/Missing").expect("path");
    assert_eq!(
        fixture
            .plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                element: absent_path,
                expansion: ElementSnapshotExpansion::default(),
            })
            .await,
        Err(AccessibilityPlaneError::NotFound)
    );

    let mut wrong_bus = element_by_name(&fixture, "Ready").await;
    wrong_bus.application.unique_bus_name = AtspiBusName::new(":9.9").expect("bus");
    assert_eq!(
        fixture
            .plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                element: wrong_bus,
                expansion: ElementSnapshotExpansion::default(),
            })
            .await,
        Err(AccessibilityPlaneError::NotFound)
    );
}

#[tokio::test]
async fn action_target_boundary_returns_only_current_raw_identity_evidence() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let old = element_by_name(&fixture, "Ready").await;
    let evidence = fixture
        .plane
        .resolve_action_target(&old)
        .await
        .expect("exact action target");
    assert_eq!(evidence.object(), &address("/org/example/App/Ready"));
    assert_eq!(evidence.application(), &address(APP_PATH));
    assert_eq!(evidence.accessibility_generation(), 1);
    assert_eq!(evidence.application_generation(), 1);
    assert_eq!(evidence.source_revision(), 1);
    assert_eq!(evidence.node_revision(), 1);
    assert_eq!(evidence.current_element(), &old);
    assert!(evidence.cache_revision().get() > 0);
    let actor_request = evidence.semantic_target_request();
    assert_eq!(actor_request.object, *evidence.object());
    assert_eq!(actor_request.application, *evidence.application());
    assert_eq!(actor_request.accessibility_generation, 1);
    assert_eq!(actor_request.application_generation, 1);
    assert_eq!(actor_request.cache_revision, 1);
    assert_eq!(actor_request.node_revision, 1);

    let mut unrelated = old.clone();
    unrelated.object_path = AtspiObjectPath::new("/org/example/App/Missing").expect("path");
    assert_eq!(
        fixture.plane.resolve_action_target(&unrelated).await,
        Err(AccessibilityPlaneError::NotFound)
    );

    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("same-path rebirth");
    assert!(matches!(
        fixture.plane.resolve_action_target(&old).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
    let successor = element_by_name(&fixture, "Ready").await;
    assert_ne!(successor, old);
    assert_eq!(
        fixture
            .plane
            .resolve_action_target(&successor)
            .await
            .expect("successor target")
            .node_revision(),
        2
    );
}

#[tokio::test]
async fn action_target_boundary_fails_closed_while_resync_is_pending() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let element = element_by_name(&fixture, "Ready").await;
    fixture
        .plane
        .begin_resync(2, AccessibilityResyncCause::ActorSignal)
        .await
        .expect("begin resync");
    assert!(matches!(
        fixture.plane.resolve_action_target(&element).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn action_target_revalidation_rejects_unrelated_global_revision_drift() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let alpha = element_by_name(&fixture, "Alpha").await;
    let old = fixture
        .plane
        .resolve_action_target(&alpha)
        .await
        .expect("initial evidence");
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("unrelated mutation");

    let fresh = fixture
        .plane
        .resolve_action_target(&alpha)
        .await
        .expect("unchanged exact birth remains live");
    assert_eq!(fresh.current_element(), &alpha);
    assert_ne!(fresh.source_revision(), old.source_revision());
    assert_ne!(fresh.cache_revision(), old.cache_revision());
    assert!(matches!(
        fixture.plane.revalidate_action_target(&old).await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
    assert_eq!(
        fixture
            .plane
            .revalidate_action_target(&fresh)
            .await
            .expect("fresh evidence"),
        fresh
    );
}

#[tokio::test]
async fn wait_honors_after_revision_timeout_resync_admission_and_cancellation() {
    let config = AccessibilityPlaneConfig {
        max_pending_waits: 1,
        ..AccessibilityPlaneConfig::default()
    };
    let fixture = Fixture::new(config);
    fixture.bootstrap(basic_nodes()).await;
    let revision = fixture
        .plane
        .list_for("alice", fixture.list(10))
        .await
        .expect("snapshot")
        .snapshot_revision;
    let wait = fixture.wait(Some(revision), 1_000);
    let task = {
        let plane = Arc::clone(&fixture.plane);
        tokio::spawn(async move { plane.wait_for(wait).await })
    };
    yield_now().await;
    assert_eq!(
        fixture.plane.wait_for(fixture.wait(None, 10)).await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("refresh");
    let matched = task.await.expect("join").expect("wait");
    assert_eq!(matched.status, ElementWaitStatus::Matched);
    assert!(matched.evaluated_revision > revision);

    let timeout = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 5)
        })
        .await
        .expect("timeout result");
    assert_eq!(timeout.status, ElementWaitStatus::TimedOut);

    let pending = {
        let plane = Arc::clone(&fixture.plane);
        let request = ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 1_000)
        };
        tokio::spawn(async move { plane.wait_for(request).await })
    };
    yield_now().await;
    pending.abort();
    let _ = pending.await;
    yield_now().await;
    let admitted = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 5)
        })
        .await
        .expect("cancel released admission");
    assert_eq!(admitted.status, ElementWaitStatus::TimedOut);

    let pending = {
        let plane = Arc::clone(&fixture.plane);
        let request = ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 1_000)
        };
        tokio::spawn(async move { plane.wait_for(request).await })
    };
    yield_now().await;
    fixture
        .plane
        .ingest_mutation(
            1,
            2,
            CacheMutation {
                revision: 3,
                kind: CacheMutationKind::ResyncRequired,
                detail: CacheMutationDetail::ResyncRequired,
            },
        )
        .await
        .expect("barrier event");
    let result = pending.await.expect("join").expect("wait");
    assert_eq!(result.status, ElementWaitStatus::ResyncRequired);
}

#[tokio::test]
async fn query_timeout_is_enforced_inside_blocking_evaluation() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    fixture.plane.set_read_test_hook(Some(Arc::new(|| {
        std::thread::sleep(Duration::from_millis(10));
    })));
    let mut request = fixture.list(10);
    request.limits.timeout_ms = 1;
    let result = fixture.plane.list_for("alice", request).await;
    fixture.plane.set_read_test_hook(None);
    assert_eq!(result, Err(AccessibilityPlaneError::QueryLimitExceeded));
}

#[tokio::test]
async fn tiny_outer_wait_timeout_remains_a_normal_wait_result() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    fixture.plane.set_read_test_hook(Some(Arc::new(|| {
        std::thread::sleep(Duration::from_millis(10));
    })));
    let result = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 1)
        })
        .await;
    fixture.plane.set_read_test_hook(None);
    assert_eq!(
        result.expect("outer timeout result").status,
        ElementWaitStatus::TimedOut
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_deadline_is_checked_before_retrying_a_stale_revision() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    fixture.plane.set_read_test_hook(Some(Arc::new({
        let invocations = Arc::clone(&invocations);
        let release_rx = Arc::clone(&release_rx);
        move || {
            if invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                started_tx.send(()).expect("started");
                release_rx
                    .lock()
                    .expect("release lock")
                    .recv()
                    .expect("release");
            }
        }
    })));
    let wait = ElementWaitRequest {
        target: ElementWaitTarget::Selector {
            selector: fixture.selector(vec![ElementPredicate::Name {
                matcher: ElementStringMatch::Exact {
                    value: "Never".to_owned(),
                    case_sensitive: true,
                },
            }]),
            quantifier: ElementWaitQuantifier::Any,
        },
        ..fixture.wait(None, 50)
    };
    let task = {
        let plane = Arc::clone(&fixture.plane);
        tokio::spawn(async move { plane.wait_for(wait).await })
    };
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("evaluation started");
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("revision churn");
    let state_guard = fixture.plane.state.lock().await;
    release_tx.send(()).expect("release evaluation");
    tokio::time::sleep(Duration::from_millis(70)).await;
    drop(state_guard);
    let result = task.await.expect("join").expect("wait result");
    fixture.plane.set_read_test_hook(None);
    assert_eq!(result.status, ElementWaitStatus::TimedOut);
    assert_eq!(
        invocations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an expired wait must not start another snapshot evaluation"
    );
}

#[tokio::test]
async fn saturated_read_admission_does_not_consume_a_cursor() {
    let fixture = Fixture::new(AccessibilityPlaneConfig {
        max_pending_reads: 1,
        ..AccessibilityPlaneConfig::default()
    });
    fixture.bootstrap(basic_nodes()).await;
    let cursor = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("first page")
        .next_cursor
        .expect("cursor");
    let mut continuation = fixture.list(1);
    continuation.cursor = Some(cursor);
    let permit = fixture
        .plane
        .acquire_read_permit()
        .expect("reserve only read slot");
    assert_eq!(
        fixture.plane.list_for("alice", continuation.clone()).await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    drop(permit);
    assert!(fixture.plane.list_for("alice", continuation).await.is_ok());
}

#[tokio::test]
async fn exact_reference_poll_reconciles_without_holding_the_plane_lock() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    fixture
        .plane
        .set_poll_policy(AccessibilityPollPolicy {
            initial_backoff: Duration::from_millis(1),
            maximum_backoff: Duration::from_millis(4),
            max_attempts: 2,
        })
        .expect("poll policy");
    let element = element_by_name(&fixture, "Ready").await;
    let fake = Arc::new(FakePollReconciler::new(
        &fixture.plane,
        [FakePollResponse::MirrorCommit {
            dispatch: AccessibilityPollDispatch {
                accessibility_generation: 1,
                source_revision: 1,
                node_revision: 1,
            },
            previous_revision: 1,
            mutation: CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Removed,
                detail: CacheMutationDetail::Removed(vec![address("/org/example/App/Ready")]),
            },
        }],
    ));
    fixture.plane.set_poll_reconciler(Some(fake.clone()));
    let result = fixture
        .plane
        .wait_for(ElementWaitRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            target: ElementWaitTarget::Reference {
                element: element.clone(),
            },
            predicate: ElementWaitPredicate::Gone,
            after_revision: None,
            timeout_ms: 100,
            allow_poll_fallback: true,
            expansion: ElementSnapshotExpansion::default(),
            limits: AccessibilityQueryLimits::default(),
        })
        .await
        .expect("poll wait");
    fixture.plane.set_poll_reconciler(None);
    assert_eq!(result.status, ElementWaitStatus::Matched);
    assert!(result.poll_fallback_used);
    assert!(fake.plane_lock_was_free.load(Ordering::SeqCst));
    {
        let captured = fake.evidence.lock().expect("evidence lock");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].current_element(), &element);
    }
    fixture
        .plane
        .ingest_mutation(
            1,
            2,
            CacheMutation {
                revision: 3,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Zulu",
                    Some(APP_PATH),
                    "Zulu",
                    43,
                    3,
                ))),
            },
        )
        .await
        .expect("ordinary actor event after poll refresh");
    assert!(
        fixture
            .plane
            .list_for("alice", fixture.list(10))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn exact_snapshot_hydrates_missing_component_evidence_before_projection() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let alpha = nodes
        .iter_mut()
        .find(|node| node.item.name == "Alpha")
        .expect("alpha");
    alpha
        .item
        .interfaces
        .push("org.a11y.atspi.Component".to_owned());
    alpha.identity_fingerprint = alpha.item.identity_fingerprint();
    fixture.bootstrap(nodes).await;
    let reference = fixture
        .plane
        .resolve_for(ElementResolveRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            selector: fixture.selector(vec![ElementPredicate::Name {
                matcher: ElementStringMatch::Exact {
                    value: "Alpha".to_owned(),
                    case_sensitive: true,
                },
            }]),
            expansion: ElementSnapshotExpansion {
                component: false,
                ..ElementSnapshotExpansion::default()
            },
            limits: AccessibilityQueryLimits::default(),
        })
        .await
        .expect("structural resolution")
        .element
        .snapshot
        .element;
    let mut refreshed = cached("/org/example/App/Alpha", Some(APP_PATH), "Alpha", 43, 2);
    refreshed
        .item
        .interfaces
        .push("org.a11y.atspi.Component".to_owned());
    refreshed.identity_fingerprint = refreshed.item.identity_fingerprint();
    refreshed.live.bounds = Some(SemanticRect {
        x: 20,
        y: 30,
        width: 40,
        height: 50,
    });
    let fake = Arc::new(FakePollReconciler::new(
        &fixture.plane,
        [FakePollResponse::MirrorCommit {
            dispatch: AccessibilityPollDispatch {
                accessibility_generation: 1,
                source_revision: 1,
                node_revision: 1,
            },
            previous_revision: 1,
            mutation: CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Refreshed,
                detail: CacheMutationDetail::Refreshed(Box::new(refreshed)),
            },
        }],
    ));
    fixture.plane.set_poll_reconciler(Some(fake.clone()));
    let result = fixture
        .plane
        .snapshot_for(ElementSnapshotRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            element: reference.clone(),
            expansion: ElementSnapshotExpansion::default(),
        })
        .await
        .expect("hydrated snapshot");
    fixture.plane.set_poll_reconciler(None);
    assert_eq!(result.element.snapshot.element, reference);
    let component = result
        .element
        .snapshot
        .component
        .expect("component evidence");
    assert_eq!(
        component.extents,
        Some(Rect::new(20, 30, 40, 50).expect("extents"))
    );
    assert!(fake.plane_lock_was_free.load(Ordering::SeqCst));
}

#[tokio::test]
async fn exact_value_wait_hydrates_predicate_evidence_not_only_projection() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let alpha = nodes
        .iter_mut()
        .find(|node| node.item.name == "Alpha")
        .expect("alpha");
    alpha
        .item
        .interfaces
        .push("org.a11y.atspi.Value".to_owned());
    alpha.identity_fingerprint = alpha.item.identity_fingerprint();
    fixture.bootstrap(nodes).await;
    let element = element_by_name(&fixture, "Alpha").await;

    let mut refreshed = cached("/org/example/App/Alpha", Some(APP_PATH), "Alpha", 43, 2);
    refreshed
        .item
        .interfaces
        .push("org.a11y.atspi.Value".to_owned());
    refreshed.identity_fingerprint = refreshed.item.identity_fingerprint();
    refreshed.live.value = Some(SemanticValueEvidence {
        current: 5.0,
        minimum: 0.0,
        maximum: 10.0,
        minimum_increment: 1.0,
    });
    let fake = Arc::new(FakePollReconciler::new(
        &fixture.plane,
        [FakePollResponse::MirrorCommit {
            dispatch: AccessibilityPollDispatch {
                accessibility_generation: 1,
                source_revision: 1,
                node_revision: 1,
            },
            previous_revision: 1,
            mutation: CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Refreshed,
                detail: CacheMutationDetail::Refreshed(Box::new(refreshed)),
            },
        }],
    ));
    fixture.plane.set_poll_reconciler(Some(fake.clone()));
    let result = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Reference { element },
            predicate: ElementWaitPredicate::Value {
                minimum: Some(4.0),
                maximum: Some(6.0),
            },
            allow_poll_fallback: true,
            expansion: ElementSnapshotExpansion {
                component: false,
                ..ElementSnapshotExpansion::default()
            },
            ..fixture.wait(None, 100)
        })
        .await
        .expect("hydrated value wait");
    fixture.plane.set_poll_reconciler(None);
    assert_eq!(result.status, ElementWaitStatus::Matched);
    assert!(result.poll_fallback_used);
    assert_eq!(fake.calls.lock().expect("call lock").len(), 1);
}

#[tokio::test]
async fn zero_sized_component_projects_as_known_component_without_extents() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut nodes = basic_nodes();
    let alpha = nodes
        .iter_mut()
        .find(|node| node.item.name == "Alpha")
        .expect("alpha");
    alpha
        .item
        .interfaces
        .push("org.a11y.atspi.Component".to_owned());
    alpha.identity_fingerprint = alpha.item.identity_fingerprint();
    alpha.live.bounds = Some(SemanticRect {
        x: 20,
        y: 30,
        width: 0,
        height: 0,
    });
    fixture.bootstrap(nodes).await;
    let element = element_by_name(&fixture, "Alpha").await;
    let result = fixture
        .plane
        .snapshot_for(ElementSnapshotRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            element,
            expansion: ElementSnapshotExpansion::default(),
        })
        .await
        .expect("zero-sized component snapshot");
    assert_eq!(
        result
            .element
            .snapshot
            .component
            .expect("component")
            .extents,
        None
    );
}

#[tokio::test]
async fn poll_fallback_deadline_and_capacity_are_bounded_and_truthful() {
    let deadline = Fixture::new(AccessibilityPlaneConfig::default());
    deadline.bootstrap(basic_nodes()).await;
    deadline
        .plane
        .set_poll_policy(AccessibilityPollPolicy {
            initial_backoff: Duration::from_millis(1),
            maximum_backoff: Duration::from_millis(2),
            max_attempts: 2,
        })
        .expect("poll policy");
    let element = element_by_name(&deadline, "Ready").await;
    let pending = Arc::new(FakePollReconciler::new(
        &deadline.plane,
        [FakePollResponse::Pending],
    ));
    deadline.plane.set_poll_reconciler(Some(pending.clone()));
    let result = deadline
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Reference {
                element: element.clone(),
            },
            predicate: ElementWaitPredicate::Gone,
            allow_poll_fallback: true,
            ..deadline.wait(None, 15)
        })
        .await
        .expect("deadline result");
    deadline.plane.set_poll_reconciler(None);
    assert_eq!(result.status, ElementWaitStatus::TimedOut);
    assert!(result.poll_fallback_used);
    assert_eq!(pending.calls.lock().expect("call lock").len(), 1);

    let saturated = Fixture::new(AccessibilityPlaneConfig::default());
    saturated.bootstrap(basic_nodes()).await;
    saturated
        .plane
        .set_poll_policy(AccessibilityPollPolicy {
            initial_backoff: Duration::from_millis(1),
            maximum_backoff: Duration::from_millis(2),
            max_attempts: 1,
        })
        .expect("poll policy");
    let element = element_by_name(&saturated, "Ready").await;
    let fake = Arc::new(FakePollReconciler::new(
        &saturated.plane,
        [FakePollResponse::Pending],
    ));
    saturated.plane.set_poll_reconciler(Some(fake.clone()));
    let permits = u32::try_from(DEFAULT_MAX_PENDING_POLLS).expect("poll permits fit u32");
    let held = Arc::clone(&saturated.plane.poll_slots)
        .try_acquire_many_owned(permits)
        .expect("reserve poll capacity");
    assert_eq!(
        saturated
            .plane
            .wait_for(ElementWaitRequest {
                target: ElementWaitTarget::Reference { element },
                predicate: ElementWaitPredicate::Gone,
                allow_poll_fallback: true,
                ..saturated.wait(None, 100)
            })
            .await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    assert!(fake.calls.lock().expect("call lock").is_empty());
    drop(held);
    saturated.plane.set_poll_reconciler(None);
}

#[tokio::test]
async fn poll_fallback_backoff_is_attempt_bounded() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    fixture
        .plane
        .set_poll_policy(AccessibilityPollPolicy {
            initial_backoff: Duration::from_millis(2),
            maximum_backoff: Duration::from_millis(8),
            max_attempts: 3,
        })
        .expect("poll policy");
    let element = element_by_name(&fixture, "Ready").await;
    let unchanged = || {
        FakePollResponse::Immediate(Ok(AccessibilityPollDispatch {
            accessibility_generation: 1,
            source_revision: 1,
            node_revision: 1,
        }))
    };
    let fake = Arc::new(FakePollReconciler::new(
        &fixture.plane,
        [unchanged(), unchanged(), unchanged()],
    ));
    fixture.plane.set_poll_reconciler(Some(fake.clone()));
    let result = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Reference { element },
            predicate: ElementWaitPredicate::Gone,
            allow_poll_fallback: true,
            ..fixture.wait(None, 50)
        })
        .await
        .expect("bounded poll timeout");
    fixture.plane.set_poll_reconciler(None);
    assert_eq!(result.status, ElementWaitStatus::TimedOut);
    assert!(result.poll_fallback_used);
    let calls = fake.calls.lock().expect("call lock");
    assert_eq!(calls.len(), 3);
    assert!(calls[1].duration_since(calls[0]) >= Duration::from_millis(4));
    assert!(calls[2].duration_since(calls[1]) >= Duration::from_millis(8));
}

#[tokio::test]
async fn selector_poll_fallback_is_explicitly_unavailable() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let mut unsupported = fixture.wait(None, 20);
    unsupported.allow_poll_fallback = true;
    assert_eq!(
        fixture.plane.wait_for(unsupported).await,
        Err(AccessibilityPlaneError::CapabilityUnavailable)
    );

    let ordinary = fixture
        .plane
        .wait_for(ElementWaitRequest {
            target: ElementWaitTarget::Selector {
                selector: fixture.selector(vec![ElementPredicate::Name {
                    matcher: ElementStringMatch::Exact {
                        value: "Never".to_owned(),
                        case_sensitive: true,
                    },
                }]),
                quantifier: ElementWaitQuantifier::Any,
            },
            ..fixture.wait(None, 5)
        })
        .await
        .expect("ordinary event-driven selector wait");
    assert_eq!(ordinary.status, ElementWaitStatus::TimedOut);
    assert!(!ordinary.poll_fallback_used);
}

#[tokio::test]
async fn password_nodes_are_fail_closed_and_unknown_raw_values_are_tolerated() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let mut password = cached(
        "/org/example/App/Password",
        Some(APP_PATH),
        "Password",
        40,
        1,
    );
    password.item.states = state_words(&[8, 63]);
    password.item.interfaces = vec![
        "org.a11y.atspi.Text".to_owned(),
        "org.example.FutureInterface".to_owned(),
    ];
    password.identity_fingerprint = password.item.identity_fingerprint();
    fixture
        .bootstrap(vec![cached(APP_PATH, None, "Application", 75, 1), password])
        .await;
    let expansion = ElementSnapshotExpansion {
        text_metadata: true,
        value: true,
        component: false,
        ..ElementSnapshotExpansion::default()
    };
    let result = fixture
        .plane
        .resolve_for(ElementResolveRequest {
            desktop_id: fixture.desktop_id,
            desktop_generation: fixture.desktop_generation,
            selector: fixture.selector(vec![ElementPredicate::Role {
                roles: vec![ElementRole::PasswordText],
            }]),
            expansion,
            limits: AccessibilityQueryLimits::default(),
        })
        .await
        .expect("password");
    let snapshot = result.element.snapshot;
    assert!(snapshot.is_protected());
    assert!(snapshot.name.is_none());
    assert!(snapshot.description.is_none());
    assert!(snapshot.value.is_none());
    assert!(snapshot.attributes.is_empty());
    assert!(snapshot.text.is_none());
}

#[tokio::test]
async fn malformed_graph_and_bootstrap_budgets_fail_closed() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let orphan = vec![cached(
        "/org/example/App/Orphan",
        Some("/org/example/App/Missing"),
        "Orphan",
        43,
        1,
    )];
    let error = fixture
        .plane
        .ingest_cache_page(cache_page(1, 1, None, orphan, None))
        .await;
    assert!(matches!(
        error,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let limited = Fixture::new(AccessibilityPlaneConfig {
        model_limits: AccessibilityModelLimits {
            max_live_nodes: 1,
            max_tombstones: 1,
        },
        ..AccessibilityPlaneConfig::default()
    });
    assert_eq!(
        limited
            .plane
            .ingest_cache_page(cache_page(1, 1, None, basic_nodes(), None))
            .await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
}

#[tokio::test]
async fn bootstrap_pages_are_atomic_and_bound_to_one_generation_and_revision() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let root = cached(APP_PATH, None, "Application", 75, 1);
    let root_address = root.item.object.clone();
    let pending = fixture
        .plane
        .ingest_cache_page(cache_page(
            1,
            1,
            None,
            vec![root],
            Some(root_address.clone()),
        ))
        .await
        .expect("first page");
    assert_eq!(pending.kind, AccessibilityIngestKind::BootstrapPending);
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    let installed = fixture
        .plane
        .ingest_cache_page(cache_page(
            1,
            1,
            Some(root_address),
            vec![
                cached("/org/example/App/Alpha", Some(APP_PATH), "Alpha", 43, 1),
                cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 1),
            ],
            None,
        ))
        .await
        .expect("final page");
    assert_eq!(installed.kind, AccessibilityIngestKind::Rebuilt);
    assert_eq!(
        fixture
            .plane
            .list_for("alice", fixture.list(10))
            .await
            .expect("atomic tree")
            .elements
            .len(),
        3
    );
}

#[tokio::test]
async fn bootstrap_rejects_empty_or_skipped_continuation_pages() {
    let empty = Fixture::new(AccessibilityPlaneConfig::default());
    let root = cached(APP_PATH, None, "Application", 75, 1);
    let root_address = root.item.object.clone();
    empty
        .plane
        .ingest_cache_page(cache_page(
            1,
            1,
            None,
            vec![root],
            Some(root_address.clone()),
        ))
        .await
        .expect("first page");
    assert!(matches!(
        empty
            .plane
            .ingest_cache_page(CachePage {
                accessibility_generation: 1,
                revision: 1,
                event_overflow_epoch: 0,
                after: Some(root_address),
                nodes: Vec::new(),
                next_after: None,
                estimated_bytes: 0,
            })
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let skipped = Fixture::new(AccessibilityPlaneConfig::default());
    let root = cached(APP_PATH, None, "Application", 75, 1);
    let root_address = root.item.object.clone();
    skipped
        .plane
        .ingest_cache_page(cache_page(1, 1, None, vec![root], Some(root_address)))
        .await
        .expect("first page");
    assert!(matches!(
        skipped
            .plane
            .ingest_cache_page(cache_page(
                1,
                1,
                None,
                vec![cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    1,
                )],
                None,
            ))
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        skipped.plane.list_for("alice", skipped.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn explicitly_prepared_generation_replaces_a_partial_initial_bootstrap() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    let root = cached(APP_PATH, None, "Application", 75, 1);
    let root_address = root.item.object.clone();
    fixture
        .plane
        .ingest_cache_page(cache_page(1, 1, None, vec![root], Some(root_address)))
        .await
        .expect("partial initial bootstrap");
    fixture
        .plane
        .begin_resync(2, AccessibilityResyncCause::EventGap)
        .await
        .expect("prepare replacement generation");
    let rebuilt = fixture
        .plane
        .ingest_cache_page(cache_page(2, 5, None, basic_nodes(), None))
        .await
        .expect("replacement bootstrap");
    assert_eq!(rebuilt.kind, AccessibilityIngestKind::Rebuilt);
    assert_eq!(rebuilt.atspi_generation.get(), 2);
    assert_eq!(
        fixture
            .plane
            .list_for("alice", fixture.list(10))
            .await
            .expect("replacement mirror")
            .atspi_generation
            .get(),
        2
    );
}

#[tokio::test]
async fn raw_cache_limits_are_recomputed_for_bootstrap_and_incrementals() {
    let raw_limits = xenoteer_atspi::CacheLimits {
        max_states: 3,
        ..xenoteer_atspi::CacheLimits::default()
    };
    let oversized_bootstrap = Fixture::new(AccessibilityPlaneConfig {
        raw_cache_limits: raw_limits,
        ..AccessibilityPlaneConfig::default()
    });
    let mut huge = cached(APP_PATH, None, "Application", 75, 1);
    huge.item.states = vec![8; 4];
    huge.identity_fingerprint = huge.item.identity_fingerprint();
    assert_eq!(
        oversized_bootstrap
            .plane
            .ingest_cache_page(CachePage {
                accessibility_generation: 1,
                revision: 1,
                event_overflow_epoch: 0,
                after: None,
                nodes: vec![huge],
                next_after: None,
                // Deliberately underreported; daemon admission recomputes it.
                estimated_bytes: 0,
            })
            .await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );

    let oversized_incremental = Fixture::new(AccessibilityPlaneConfig {
        raw_cache_limits: raw_limits,
        ..AccessibilityPlaneConfig::default()
    });
    oversized_incremental.bootstrap(basic_nodes()).await;
    let mut huge = cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 2);
    huge.item.states = vec![8; 4];
    huge.identity_fingerprint = huge.item.identity_fingerprint();
    assert!(matches!(
        oversized_incremental
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Upserted,
                    detail: CacheMutationDetail::Upserted(Box::new(huge)),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let initial = basic_nodes();
    let initial_bytes = estimated_nodes_bytes(&initial);
    let addition = cached("/org/example/App/Zulu", Some(APP_PATH), "Zulu", 43, 2);
    let addition_bytes = checked_raw_node_bytes(&addition).expect("bounded addition");
    let aggregate = Fixture::new(AccessibilityPlaneConfig {
        raw_cache_limits: xenoteer_atspi::CacheLimits {
            max_item_bytes: initial_bytes + addition_bytes - 1,
            max_total_bytes: initial_bytes + addition_bytes - 1,
            ..xenoteer_atspi::CacheLimits::default()
        },
        ..AccessibilityPlaneConfig::default()
    });
    aggregate.bootstrap(initial).await;
    assert!(matches!(
        aggregate
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Upserted,
                    detail: CacheMutationDetail::Upserted(Box::new(addition)),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        aggregate.plane.list_for("alice", aggregate.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn window_scope_is_empty_without_evidence_then_includes_nested_descendants() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let window = WindowRef {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("a".repeat(WINDOW_IDENTITY_HASH_BYTES))
            .expect("identity"),
    };
    let mut request = fixture.list(10);
    request.scope = ElementScope::Window {
        window: window.clone(),
    };
    assert!(
        fixture
            .plane
            .list_for("alice", request.clone())
            .await
            .expect("uncorrelated")
            .elements
            .is_empty()
    );
    let targets = fixture
        .plane
        .accessibility_correlation_targets()
        .await
        .expect("correlation targets");
    let application = targets
        .targets()
        .iter()
        .find(|target| target.snapshot().role.role == ElementRole::Application)
        .expect("application target");
    let weak = ElementWindowCorrelation {
        window: Some(window.clone()),
        confidence: WindowCorrelationConfidence::Weak,
        evidence: vec![WindowCorrelationEvidence {
            signal: WindowCorrelationSignal::Title,
            matched: true,
            detail: None,
        }],
        conflicting_evidence: false,
    };
    assert!(!xenoteer_core::correlation_authorizes_physical_effect(
        &weak
    ));
    fixture
        .plane
        .replace_window_correlations(targets.fence(), vec![application.assignment(weak.clone())])
        .await
        .expect("weak reportable correlation");
    let weak_page = fixture
        .plane
        .list_for("alice", request.clone())
        .await
        .expect("weak window scope");
    assert_eq!(weak_page.elements.len(), 3);
    assert!(
        weak_page
            .elements
            .iter()
            .all(|entry| entry.snapshot.window_correlation == weak)
    );

    let targets = fixture
        .plane
        .accessibility_correlation_targets()
        .await
        .expect("refreshed correlation targets");
    let application = targets
        .targets()
        .iter()
        .find(|target| target.snapshot().role.role == ElementRole::Application)
        .expect("refreshed application target");
    fixture
        .plane
        .replace_window_correlations(
            targets.fence(),
            vec![application.assignment(ElementWindowCorrelation {
                window: Some(window),
                confidence: WindowCorrelationConfidence::Strong,
                evidence: vec![WindowCorrelationEvidence {
                    signal: WindowCorrelationSignal::ExplicitCallerReference,
                    matched: true,
                    detail: None,
                }],
                conflicting_evidence: false,
            })],
        )
        .await
        .expect("correlation");
    let page = fixture
        .plane
        .list_for("alice", request)
        .await
        .expect("window scope");
    assert_eq!(page.elements.len(), 3);
}

#[tokio::test]
async fn ambiguous_no_target_evidence_is_reported_and_empty_assignment_clears() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let targets = fixture
        .plane
        .accessibility_correlation_targets()
        .await
        .expect("targets");
    let application = targets
        .targets()
        .iter()
        .find(|target| target.snapshot().role.role == ElementRole::Application)
        .expect("application");
    let ambiguous = ElementWindowCorrelation {
        window: None,
        confidence: WindowCorrelationConfidence::None,
        evidence: vec![WindowCorrelationEvidence {
            signal: WindowCorrelationSignal::Title,
            matched: false,
            detail: Some("ambiguous bounded candidates".to_owned()),
        }],
        conflicting_evidence: true,
    };
    fixture
        .plane
        .replace_window_correlations(
            targets.fence(),
            vec![application.assignment(ambiguous.clone())],
        )
        .await
        .expect("report ambiguity");
    let reported = fixture
        .plane
        .list_for("alice", fixture.list(10))
        .await
        .expect("reported list");
    assert!(
        reported
            .elements
            .iter()
            .all(|entry| entry.snapshot.window_correlation == ambiguous)
    );
    assert!(!xenoteer_core::correlation_authorizes_physical_effect(
        &ambiguous
    ));

    let refreshed = fixture
        .plane
        .accessibility_correlation_targets()
        .await
        .expect("refreshed targets");
    fixture
        .plane
        .replace_window_correlations(refreshed.fence(), Vec::new())
        .await
        .expect("clear");
    let cleared = fixture
        .plane
        .list_for("alice", fixture.list(10))
        .await
        .expect("cleared list");
    assert!(cleared.elements.iter().all(|entry| {
        entry.snapshot.window_correlation.window.is_none()
            && entry.snapshot.window_correlation.evidence.is_empty()
            && !entry.snapshot.window_correlation.conflicting_evidence
    }));
}

#[tokio::test]
async fn correlations_are_cleared_by_removal_and_resync_barriers() {
    async fn install(fixture: &Fixture, window: WindowRef) {
        let targets = fixture
            .plane
            .accessibility_correlation_targets()
            .await
            .expect("targets");
        let application = targets
            .targets()
            .iter()
            .find(|target| target.snapshot().role.role == ElementRole::Application)
            .expect("application");
        fixture
            .plane
            .replace_window_correlations(
                targets.fence(),
                vec![application.assignment(ElementWindowCorrelation {
                    window: Some(window),
                    confidence: WindowCorrelationConfidence::Strong,
                    evidence: vec![WindowCorrelationEvidence {
                        signal: WindowCorrelationSignal::TopLevelExtents,
                        matched: true,
                        detail: None,
                    }],
                    conflicting_evidence: false,
                })],
            )
            .await
            .expect("install");
    }

    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let window = WindowRef {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("c".repeat(WINDOW_IDENTITY_HASH_BYTES))
            .expect("identity"),
    };
    install(&fixture, window.clone()).await;
    assert_eq!(fixture.plane.state.lock().await.correlations.len(), 1);
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Removed,
                detail: CacheMutationDetail::Removed(vec![
                    address(APP_PATH),
                    address("/org/example/App/Alpha"),
                    address("/org/example/App/Ready"),
                ]),
            },
        )
        .await
        .expect("removal");
    assert!(fixture.plane.state.lock().await.correlations.is_empty());

    let resync = Fixture::new(AccessibilityPlaneConfig::default());
    resync.bootstrap(basic_nodes()).await;
    let mut resync_window = window;
    resync_window.desktop_id = resync.desktop_id;
    resync_window.desktop_generation = resync.desktop_generation;
    install(&resync, resync_window).await;
    resync
        .plane
        .begin_resync(2, AccessibilityResyncCause::EventGap)
        .await
        .expect("resync");
    assert!(resync.plane.state.lock().await.correlations.is_empty());
}

#[tokio::test]
async fn actor_refresh_preserves_birth_allows_mutable_fingerprint_and_clears_target_correlation() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let before = element_by_name(&fixture, "Application").await;
    let window = WindowRef {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("d".repeat(WINDOW_IDENTITY_HASH_BYTES))
            .expect("identity"),
    };
    let targets = fixture
        .plane
        .accessibility_correlation_targets()
        .await
        .expect("targets");
    let application = targets
        .targets()
        .iter()
        .find(|target| target.snapshot().element == before)
        .expect("application");
    fixture
        .plane
        .replace_window_correlations(
            targets.fence(),
            vec![application.assignment(ElementWindowCorrelation {
                window: Some(window),
                confidence: WindowCorrelationConfidence::Weak,
                evidence: vec![WindowCorrelationEvidence {
                    signal: WindowCorrelationSignal::Title,
                    matched: true,
                    detail: None,
                }],
                conflicting_evidence: false,
            })],
        )
        .await
        .expect("correlation");

    let refreshed = cached(APP_PATH, None, "Renamed Application", 75, 2);
    assert_ne!(
        refreshed.identity_fingerprint,
        basic_nodes()[0].identity_fingerprint
    );
    let event = fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Refreshed,
                detail: CacheMutationDetail::Refreshed(Box::new(refreshed)),
            },
        )
        .await
        .expect("refresh");
    assert_eq!(event.kind, AccessibilityIngestKind::Refreshed);
    let after = element_by_name(&fixture, "Renamed Application").await;
    assert_eq!(after, before);
    let page = fixture
        .plane
        .list_for("alice", fixture.list(10))
        .await
        .expect("list");
    assert!(page.elements.iter().all(|entry| {
        entry.snapshot.window_correlation.window.is_none()
            && entry.snapshot.window_correlation.evidence.is_empty()
    }));
}

#[tokio::test]
async fn actor_refresh_with_changed_application_generation_fails_closed() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let mut refreshed = cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 2);
    refreshed.application_generation = 2;
    assert!(matches!(
        fixture
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Refreshed,
                    detail: CacheMutationDetail::Refreshed(Box::new(refreshed)),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn identical_same_path_add_mints_a_new_birth_and_stales_the_predecessor() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let predecessor = element_by_name(&fixture, "Ready").await;
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("new birth");
    let successor = element_by_name(&fixture, "Ready").await;
    assert_ne!(predecessor, successor);
    assert!(successor.cache_sequence > predecessor.cache_sequence);
    assert!(matches!(
        fixture
            .plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                element: predecessor,
                expansion: ElementSnapshotExpansion::default(),
            })
            .await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
}

#[tokio::test]
async fn application_restart_stales_old_application_instance() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let predecessor = element_by_name(&fixture, "Ready").await;
    let removed = basic_nodes()
        .into_iter()
        .map(|node| node.item.object)
        .collect();
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::ApplicationInvalidated,
                detail: CacheMutationDetail::ApplicationInvalidated {
                    bus_name: BUS.to_owned(),
                    application_generation: 2,
                    removed,
                },
            },
        )
        .await
        .expect("cache invalidation mutation");
    let duplicate = fixture
        .plane
        .ingest_application_invalidation(1, 2, BUS.to_owned(), 2)
        .await
        .expect("invalidate application");
    assert_eq!(duplicate.kind, AccessibilityIngestKind::Unchanged);
    let mut root = cached(APP_PATH, None, "Application", 75, 3);
    root.application_generation = 2;
    root.identity_fingerprint = root.item.identity_fingerprint();
    fixture
        .plane
        .ingest_mutation(
            1,
            2,
            CacheMutation {
                revision: 3,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(root)),
            },
        )
        .await
        .expect("new app root");
    let mut ready = cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 4);
    ready.application_generation = 2;
    ready.identity_fingerprint = ready.item.identity_fingerprint();
    fixture
        .plane
        .ingest_mutation(
            1,
            3,
            CacheMutation {
                revision: 4,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(ready)),
            },
        )
        .await
        .expect("new app child");
    assert!(matches!(
        fixture
            .plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: fixture.desktop_id,
                desktop_generation: fixture.desktop_generation,
                element: predecessor,
                expansion: ElementSnapshotExpansion::default(),
            })
            .await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
}

#[tokio::test]
async fn explicit_resync_preserves_expected_generation_against_delayed_events() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let old = element_by_name(&fixture, "Ready").await;
    fixture
        .plane
        .begin_resync(2, AccessibilityResyncCause::ActorSignal)
        .await
        .expect("begin resync");

    assert!(matches!(
        fixture
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Removed,
                    detail: CacheMutationDetail::Removed(vec![address("/org/example/App/Ready")]),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    let event = fixture
        .plane
        .ingest_cache_page(cache_page(2, 1, None, basic_nodes(), None))
        .await
        .expect("generation-two bootstrap");
    assert_eq!(event.atspi_generation.get(), 2);
    let successor = element_by_name(&fixture, "Ready").await;
    assert_ne!(old, successor);
    assert_eq!(successor.atspi_generation.get(), 2);

    assert!(matches!(
        fixture
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Removed,
                    detail: CacheMutationDetail::Removed(vec![address("/org/example/App/Ready")]),
                },
            )
            .await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
    assert_eq!(
        fixture
            .plane
            .list_for("alice", fixture.list(10))
            .await
            .expect("delayed event did not poison new state")
            .atspi_generation
            .get(),
        2
    );
    assert!(matches!(
        fixture
            .plane
            .ingest_cache_page(cache_page(2, 1, None, basic_nodes(), None))
            .await,
        Err(AccessibilityPlaneError::StaleReference { .. })
    ));
}

#[tokio::test]
async fn cursor_storage_is_fixed_size_collision_safe_and_debug_is_content_free() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let secret = "sensitive-selector-canary";
    let request = ElementQueryRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        selector: fixture.selector(vec![ElementPredicate::Name {
            matcher: ElementStringMatch::Exact {
                value: secret.to_owned(),
                case_sensitive: true,
            },
        }]),
        limit: 1,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    let binding = query_cursor_binding(&request).expect("binding");
    assert!(std::mem::size_of_val(&binding) < 128);
    let debug = format!("{:?}", fixture.plane);
    assert!(!debug.contains(secret));
    assert!(!debug.contains("alice"));

    let cursor = fixture
        .plane
        .list_for("alice", fixture.list(1))
        .await
        .expect("cursor page")
        .next_cursor
        .expect("cursor");
    let token = cursor.as_str().to_owned();
    let token_digest = cursor_token_digest(&token);
    assert_ne!(
        token_digest,
        request_binding_digest(b"cursor-token-v1", token.as_bytes())
    );
    assert_ne!(token_digest, cursor_token_digest("different-token"));
    let mut state = fixture.plane.state.lock().await;
    let existing = state.cursors.get(&token_digest).expect("digest record");
    let duplicate = CursorRecord {
        principal: "mallory".to_owned(),
        expires_at: existing.expires_at,
        binding: existing.binding,
        continuation: existing.continuation.clone(),
    };
    assert!(!insert_cursor_digest(&mut state, token_digest, duplicate));
    assert_eq!(
        state
            .cursors
            .get(&token_digest)
            .expect("original digest record")
            .principal,
        "alice"
    );
}

#[tokio::test]
async fn partial_mutation_failure_closes_reads_and_notifies_resync() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    let mut malformed = cached(
        "/org/example/App/Ready",
        Some(APP_PATH),
        &"x".repeat(100_000),
        43,
        2,
    );
    malformed.identity_fingerprint = malformed.item.identity_fingerprint();
    assert!(matches!(
        fixture
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Upserted,
                    detail: CacheMutationDetail::Upserted(Box::new(malformed)),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn malformed_incremental_details_and_dirty_removals_force_resync() {
    let malformed = Fixture::new(AccessibilityPlaneConfig::default());
    malformed.bootstrap(basic_nodes()).await;
    let mut poisoned = cached("/org/example/App/Ready", Some(APP_PATH), "Ready", 43, 2);
    poisoned.item.name = "fingerprint mismatch".to_owned();
    assert!(matches!(
        malformed
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Upserted,
                    detail: CacheMutationDetail::Upserted(Box::new(poisoned)),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        malformed.plane.list_for("alice", malformed.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let duplicate = Fixture::new(AccessibilityPlaneConfig::default());
    duplicate.bootstrap(basic_nodes()).await;
    let ready = address("/org/example/App/Ready");
    assert!(matches!(
        duplicate
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Removed,
                    detail: CacheMutationDetail::Removed(vec![ready.clone(), ready]),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));

    let orphan = Fixture::new(AccessibilityPlaneConfig::default());
    orphan.bootstrap(basic_nodes()).await;
    assert!(matches!(
        orphan
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::Removed,
                    detail: CacheMutationDetail::Removed(vec![address(APP_PATH)]),
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test]
async fn application_invalidation_requires_exact_generation_and_removed_set() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    fixture.bootstrap(basic_nodes()).await;
    assert!(matches!(
        fixture
            .plane
            .ingest_mutation(
                1,
                1,
                CacheMutation {
                    revision: 2,
                    kind: CacheMutationKind::ApplicationInvalidated,
                    detail: CacheMutationDetail::ApplicationInvalidated {
                        bus_name: BUS.to_owned(),
                        application_generation: 9,
                        removed: vec![address(APP_PATH)],
                    },
                },
            )
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_read_permit_survives_cancellation_and_releases_on_panic() {
    let fixture = Fixture::new(AccessibilityPlaneConfig {
        max_pending_reads: 1,
        ..AccessibilityPlaneConfig::default()
    });
    fixture.bootstrap(basic_nodes()).await;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task = {
        let plane = Arc::clone(&fixture.plane);
        tokio::spawn(async move {
            plane
                .run_bounded_read(move || {
                    started_tx.send(()).expect("started");
                    release_rx.recv().expect("release");
                })
                .await
        })
    };
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocking read started");
    task.abort();
    let _ = task.await;
    assert_eq!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResourceExhausted)
    );
    // Ingestion owns only the short state lock and progresses while the
    // cancelled blocking read still owns its CPU-admission permit.
    fixture
        .plane
        .ingest_mutation(
            1,
            1,
            CacheMutation {
                revision: 2,
                kind: CacheMutationKind::Upserted,
                detail: CacheMutationDetail::Upserted(Box::new(cached(
                    "/org/example/App/Ready",
                    Some(APP_PATH),
                    "Ready",
                    43,
                    2,
                ))),
            },
        )
        .await
        .expect("ingest progressed");
    release_tx.send(()).expect("release send");
    for _ in 0..20 {
        if fixture.plane.read_slots.available_permits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(fixture.plane.read_slots.available_permits(), 1);
    assert_eq!(
        fixture
            .plane
            .run_bounded_read(|| -> u8 { panic!("permit-release-canary") })
            .await,
        Err(AccessibilityPlaneError::Internal)
    );
    assert_eq!(fixture.plane.run_bounded_read(|| 7_u8).await, Ok(7));
}

#[tokio::test]
async fn cyclic_authoritative_topology_is_never_queryable() {
    let fixture = Fixture::new(AccessibilityPlaneConfig::default());
    assert!(matches!(
        fixture
            .plane
            .ingest_cache_page(cache_page(
                1,
                1,
                None,
                vec![
                    cached("/org/example/App/A", Some("/org/example/App/B"), "A", 43, 1,),
                    cached("/org/example/App/B", Some("/org/example/App/A"), "B", 43, 1,),
                ],
                None,
            ))
            .await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
    assert!(matches!(
        fixture.plane.list_for("alice", fixture.list(10)).await,
        Err(AccessibilityPlaneError::ResyncRequired { .. })
    ));
}
