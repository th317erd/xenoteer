//! Adversarial tests for advisory processd/window PID correlation.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use xenoteer_processd::{
    BrokerPidCorrelation, BrokerPidCorrelationEvidence, MAX_PROCESS_CORRELATION_PIDS,
};
use xenoteer_protocol::{
    CoordinateSpace, LaunchId, ProcessRef, Rect, WindowIdentityHash, WindowListPage,
    WindowMapState, WindowModelRevision, WindowPredicate, WindowQueryPage, WindowQueryRequest,
    WindowRect, WindowReferenceToken, WindowResolveRequest, WindowResolveResult, WindowSelector,
    WindowSingleMatchPolicy, WindowSnapshotResult, WindowStringMatch, WindowText, WindowTextField,
    WindowWaitPredicate, WindowWaitSelectorQuantifier, WindowWaitTarget,
};
use xenoteer_x11::{
    FocusAncestryStatus, RootGeometryInput, RootWindowEvidenceInput, WindowAttributeInput,
    WindowPropertyInput,
};

use super::*;

struct ScriptedPidCorrelator {
    calls: Mutex<Vec<(DesktopGeneration, Vec<u32>)>>,
    replies: Mutex<VecDeque<Result<Vec<BrokerPidCorrelation>, PidCorrelationError>>>,
}

#[derive(Default)]
struct PendingPidCorrelator {
    calls: Mutex<Vec<Vec<u32>>>,
}

struct ControlledPidCorrelator {
    calls: AtomicUsize,
    called: tokio::sync::Notify,
    released: AtomicBool,
    release: tokio::sync::Notify,
    reply: Result<Vec<BrokerPidCorrelation>, PidCorrelationError>,
}

impl ControlledPidCorrelator {
    fn new(reply: Result<Vec<BrokerPidCorrelation>, PidCorrelationError>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            called: tokio::sync::Notify::new(),
            released: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            reply,
        }
    }

    async fn wait_until_called(&self) {
        let called = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notified = self.called.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.calls.load(Ordering::Acquire) != 0 {
                    break;
                }
                notified.await;
            }
        })
        .await;
        assert!(
            called.is_ok(),
            "controlled correlator was not called within ten seconds"
        );
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }
}

struct SingleWindowBackend {
    snapshot: WindowSnapshotInput,
}

#[derive(Default)]
struct MutableWindowBackendState {
    windows: BTreeMap<u32, WindowSnapshotInput>,
    events: VecDeque<ObservationActorEvent>,
}

struct MutableWindowBackend {
    state: Arc<Mutex<MutableWindowBackendState>>,
}

struct DelayedMutableWindowBackend {
    state: Arc<Mutex<MutableWindowBackendState>>,
    delay_ms: Arc<AtomicU64>,
}

#[derive(Default)]
struct RecordingWindowEventSink {
    events: Mutex<Vec<NormalizedEvent>>,
}

impl RecordingWindowEventSink {
    fn take(&self) -> Vec<NormalizedEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl WindowEventSink for RecordingWindowEventSink {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}

impl RawObservationBackend for SingleWindowBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        Ok(RootInventory {
            windows: vec![self.snapshot.window],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        })
    }

    fn snapshot(
        &mut self,
        _window: u32,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        Ok(self.snapshot.clone())
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        Ok(None)
    }

    fn shutdown(&mut self, _timeout: std::time::Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

impl RawObservationBackend for MutableWindowBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(RootInventory {
            windows: state.windows.keys().copied().collect(),
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        })
    }

    fn snapshot(
        &mut self,
        window: u32,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .windows
            .get(&window)
            .cloned()
            .ok_or(RawBackendFailure::RequestFailed)
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .pop_front())
    }

    fn shutdown(&mut self, _timeout: std::time::Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

impl RawObservationBackend for DelayedMutableWindowBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        let delay_ms = self.delay_ms.load(Ordering::Acquire);
        if delay_ms != 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(RootInventory {
            windows: state.windows.keys().copied().collect(),
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        })
    }

    fn snapshot(
        &mut self,
        window: u32,
        _control: &ModelActorControl,
        _timeout: std::time::Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        let delay_ms = self.delay_ms.load(Ordering::Acquire);
        if delay_ms != 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .windows
            .get(&window)
            .cloned()
            .ok_or(RawBackendFailure::RequestFailed)
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .pop_front())
    }

    fn shutdown(&mut self, _timeout: std::time::Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

impl PidCorrelator for PendingPidCorrelator {
    fn correlate<'a>(&'a self, _: DesktopGeneration, pids: Vec<u32>) -> PidCorrelationFuture<'a> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(pids);
        Box::pin(std::future::pending())
    }
}

impl PidCorrelator for ControlledPidCorrelator {
    fn correlate<'a>(&'a self, _: DesktopGeneration, _: Vec<u32>) -> PidCorrelationFuture<'a> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.called.notify_waiters();
        Box::pin(async move {
            loop {
                let notified = self.release.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.released.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            self.reply.clone()
        })
    }
}

impl ScriptedPidCorrelator {
    fn new(
        replies: impl IntoIterator<Item = Result<Vec<BrokerPidCorrelation>, PidCorrelationError>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
        }
    }

    fn calls(&self) -> Vec<(DesktopGeneration, Vec<u32>)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl PidCorrelator for ScriptedPidCorrelator {
    fn correlate<'a>(
        &'a self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> PidCorrelationFuture<'a> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((desktop_generation, pids));
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(Err(PidCorrelationError));
        Box::pin(async move { reply })
    }
}

fn raw(window: u32, pid: u32) -> Result<WindowSnapshotInput, Box<dyn Error>> {
    Ok(WindowSnapshotInput {
        window,
        attributes: WindowAttributeInput {
            map_state: WindowMapState::Viewable,
            override_redirect: false,
            input_only: false,
            visual: 24,
            colormap: 9,
        },
        properties: WindowPropertyInput {
            title: None,
            visible_title: None,
            icon_title: None,
            class: None,
            client_machine: None,
            window_types: Vec::new(),
            states: Vec::new(),
            allowed_actions: Vec::new(),
            protocols: Vec::new(),
            reported_pid: Some(pid),
            workspace: None,
            frame_extents: None,
            client_leader: None,
            transient_for: None,
            group_leader: None,
            urgent: false,
            warnings: Vec::new(),
            warnings_truncated: false,
        },
        geometry: RootGeometryInput {
            client_rect: WindowRect::new(
                CoordinateSpace::RootPhysical,
                Rect::new(10, 20, 640, 480)?,
            )?,
            border_width: 0,
            geometry_root: 1,
            root_child: None,
        },
        root: RootWindowEvidenceInput {
            active_window: None,
            raw_focused_window: None,
            focused_window: None,
            target_contains_focus: false,
            focus_ancestry_status: FocusAncestryStatus::NoFocus,
            current_workspace: Some(0),
        },
    })
}

fn entry(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    xid: u32,
    pid: u32,
) -> Result<WindowSnapshotEntry, Box<dyn Error>> {
    let reference = WindowRef {
        desktop_id,
        desktop_generation: generation,
        xid,
        observed_generation: u64::from(xid),
        identity_hash: WindowIdentityHash::new(format!("{xid:064x}"))?,
    };
    let snapshot = normalize_snapshot(
        &raw(xid, pid)?,
        reference,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;
    Ok(WindowSnapshotEntry {
        snapshot,
        reference_token: WindowReferenceToken::new(format!("A_window_reference_{xid}"))?,
    })
}

fn process(generation: DesktopGeneration, pid: u32) -> ProcessRef {
    ProcessRef {
        desktop_generation: generation,
        pid,
        proc_start_ticks: u64::from(pid) + 100,
        launch_id: LaunchId::new(),
    }
}

fn leader(pid: u32, process: ProcessRef) -> BrokerPidCorrelation {
    BrokerPidCorrelation {
        pid,
        evidence: BrokerPidCorrelationEvidence::ManagedLeader { process },
    }
}

fn group(pid: u32, process: ProcessRef) -> BrokerPidCorrelation {
    BrokerPidCorrelation {
        pid,
        evidence: BrokerPidCorrelationEvidence::ManagedProcessGroup { process },
    }
}

fn no_match(pid: u32) -> BrokerPidCorrelation {
    BrokerPidCorrelation {
        pid,
        evidence: BrokerPidCorrelationEvidence::NoMatch,
    }
}

fn assert_low(entry: &WindowSnapshotEntry, pid: u32) {
    assert_eq!(entry.snapshot.process.reported_pid, Some(pid));
    assert_eq!(entry.snapshot.process.managed_process, None);
    assert_eq!(
        entry.snapshot.process.confidence,
        WindowProcessConfidence::Low
    );
    assert_eq!(
        entry.snapshot.process.evidence,
        vec![WindowProcessEvidence::NetWmPid]
    );
}

async fn race_unavailable_refresh_with_recommit<T, F>(
    service: &Arc<DaemonObservationService>,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    managed: ProcessRef,
    operation: F,
) -> Result<T, ControlPlaneError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, ControlPlaneError>> + Send + 'static,
{
    service.process_lifecycle_authority.disable();
    let hook = Arc::new(UnavailableRefreshHook::new());
    *service
        .unavailable_refresh_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&hook));
    let observed = hook.observed.notified();
    tokio::pin!(observed);
    observed.as_mut().enable();
    let operation = tokio::spawn(operation);
    if tokio::time::timeout(std::time::Duration::from_secs(10), observed)
        .await
        .is_err()
    {
        return Err(ControlPlaneError::CapabilityUnavailable);
    }

    service.enable_process_lifecycle_authority();
    service
        .query_for_principal(
            "recommit".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    hook.release();
    tokio::time::timeout(std::time::Duration::from_secs(10), operation)
        .await
        .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
        .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
}

fn title_and_managed_process_selector(process: ProcessRef) -> WindowSelector {
    WindowSelector::All {
        selectors: vec![
            WindowSelector::Predicate {
                predicate: WindowPredicate::Text {
                    field: WindowTextField::Title,
                    matcher: WindowStringMatch::Exact {
                        value: "Managed editor".to_owned(),
                        case_sensitive: true,
                    },
                },
            },
            WindowSelector::Predicate {
                predicate: WindowPredicate::ManagedProcess { process },
            },
        ],
    }
}

fn titled_raw(window: u32, pid: u32, title: &str) -> Result<WindowSnapshotInput, Box<dyn Error>> {
    let mut input = raw(window, pid)?;
    input.properties.title = Some(WindowText::new(title, false)?);
    Ok(input)
}

fn managed_query(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    process: ProcessRef,
) -> WindowQueryRequest {
    WindowQueryRequest {
        desktop_id,
        desktop_generation: generation,
        selector: title_and_managed_process_selector(process),
        order: WindowOrder::XidAscending,
        limit: MAX_WINDOW_PAGE_LIMIT,
        cursor: None,
    }
}

fn managed_wait(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    process: ProcessRef,
) -> WindowWaitRequest {
    WindowWaitRequest {
        desktop_id,
        desktop_generation: generation,
        target: WindowWaitTarget::Selector {
            selector: title_and_managed_process_selector(process),
            quantifier: WindowWaitSelectorQuantifier::Any,
        },
        predicate: WindowWaitPredicate::Exists,
        after_revision: None,
        timeout_ms: 1_000,
    }
}

fn title_selector() -> WindowSelector {
    WindowSelector::Predicate {
        predicate: WindowPredicate::Text {
            field: WindowTextField::Title,
            matcher: WindowStringMatch::Exact {
                value: "Managed editor".to_owned(),
                case_sensitive: true,
            },
        },
    }
}

fn title_wait(desktop_id: DesktopId, generation: DesktopGeneration) -> WindowWaitRequest {
    WindowWaitRequest {
        desktop_id,
        desktop_generation: generation,
        target: WindowWaitTarget::Selector {
            selector: title_selector(),
            quantifier: WindowWaitSelectorQuantifier::Any,
        },
        predicate: WindowWaitPredicate::Exists,
        after_revision: None,
        timeout_ms: 1_000,
    }
}

#[test]
fn committed_process_correlation_drives_query_resolve_and_wait_before_projection()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let now = MonotonicMillis::new(1);
    let mut state = ModelState::new(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let mut input = raw(42, 101)?;
    input.properties.title = Some(WindowText::new("Managed editor", false)?);
    state.reconcile_raw(
        &RootInventory {
            windows: vec![42],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        },
        std::slice::from_ref(&input),
        now,
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let correlation_snapshot = state.correlation_snapshot(now)?;
    let window = correlation_snapshot.windows[0].window.clone();
    let committed_fence = state.replace_process_correlations(
        correlation_snapshot.revision,
        correlation_snapshot.correlation_epoch,
        generation,
        &[ProcessCorrelationAssignment {
            window,
            reported_pid: Some(101),
            upgrade: Some(ProcessCorrelationUpgrade {
                process: managed,
                evidence: WindowProcessEvidence::ProcStartTime,
            }),
        }],
        now,
    )?;
    assert!(committed_fence.revision > correlation_snapshot.revision);

    let selector = title_and_managed_process_selector(managed);
    let query = state.query(
        "alice",
        &WindowQueryRequest {
            desktop_id,
            desktop_generation: generation,
            selector: selector.clone(),
            order: WindowOrder::XidAscending,
            limit: MAX_WINDOW_PAGE_LIMIT,
            cursor: None,
        },
        now,
    )?;
    assert_eq!(query.windows.len(), 1);
    assert_eq!(
        query.windows[0].snapshot.process.managed_process,
        Some(managed)
    );

    let resolved = state.resolve(
        "alice",
        &WindowResolveRequest {
            desktop_id,
            desktop_generation: generation,
            selector: selector.clone(),
            order: WindowOrder::XidAscending,
            match_policy: WindowSingleMatchPolicy::ExactlyOne,
        },
        now,
    )?;
    assert_eq!(resolved.window.snapshot.window.xid, 42);

    let waited = state
        .evaluate_wait(
            "alice",
            &WindowWaitRequest {
                desktop_id,
                desktop_generation: generation,
                target: WindowWaitTarget::Selector {
                    selector,
                    quantifier: WindowWaitSelectorQuantifier::Any,
                },
                predicate: WindowWaitPredicate::Exists,
                after_revision: None,
                timeout_ms: 100,
            },
            now,
            None,
        )?
        .ok_or("committed correlation did not satisfy wait")?;
    assert_eq!(waited.status, WindowWaitStatus::Matched);
    assert_eq!(waited.windows.len(), 1);
    assert_eq!(
        waited.windows[0].snapshot.process.managed_process,
        Some(managed)
    );
    Ok(())
}

#[test]
fn individual_observations_preserve_exact_process_evidence_and_fence_pid_mutation()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let sink = Arc::new(RecordingWindowEventSink::default());
    let mut state = ModelState::new_with_event_sink(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
        sink.clone(),
    )?;
    let initial = titled_raw(42, 101, "Managed editor")?;
    state.reconcile_raw(
        &RootInventory {
            windows: vec![42],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        },
        std::slice::from_ref(&initial),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let correlation = state.correlation_snapshot(MonotonicMillis::new(2))?;
    state.replace_process_correlations(
        correlation.revision,
        correlation.correlation_epoch,
        generation,
        &[ProcessCorrelationAssignment {
            window: correlation.windows[0].window.clone(),
            reported_pid: Some(101),
            upgrade: Some(ProcessCorrelationUpgrade {
                process: managed,
                evidence: WindowProcessEvidence::ProcStartTime,
            }),
        }],
        MonotonicMillis::new(2),
    )?;
    sink.take();
    let correlated_epoch = state.correlation_epoch;

    let unrelated = titled_raw(42, 101, "Renamed editor")?;
    state.observe_raw(&unrelated, MonotonicMillis::new(3))?;
    let after_metadata = state.correlation_snapshot(MonotonicMillis::new(3))?;
    assert_eq!(
        after_metadata.windows[0].process.managed_process,
        Some(managed)
    );
    assert_eq!(state.correlation_epoch, correlated_epoch);
    let events = sink.take();
    assert_eq!(events.len(), 1);
    let event: WindowMetadataEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(event.changed, vec![WindowMetadataField::Title]);

    let changed_pid = titled_raw(42, 202, "Renamed editor")?;
    state.observe_raw(&changed_pid, MonotonicMillis::new(4))?;
    let after_pid = state.correlation_snapshot(MonotonicMillis::new(4))?;
    assert_eq!(after_pid.windows[0].process.reported_pid, Some(202));
    assert_eq!(after_pid.windows[0].process.managed_process, None);
    assert_eq!(
        after_pid.windows[0].process.confidence,
        WindowProcessConfidence::Low
    );
    assert!(state.correlation_epoch > correlated_epoch);
    assert!(!state.process_correlation_available);
    Ok(())
}

#[tokio::test]
async fn service_query_and_resolve_evaluate_managed_process_from_committed_model()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let query = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    assert_eq!(query.windows.len(), 1);
    assert_eq!(
        query.windows[0].snapshot.process.managed_process,
        Some(managed)
    );
    let resolved = service
        .resolve_for_principal(
            "alice".to_owned(),
            WindowResolveRequest {
                desktop_id,
                desktop_generation: generation,
                selector: title_and_managed_process_selector(managed),
                order: WindowOrder::XidAscending,
                match_policy: WindowSingleMatchPolicy::ExactlyOne,
            },
        )
        .await?;
    assert_eq!(resolved.window.snapshot.window.xid, 42);
    assert_eq!(
        resolved.window.snapshot.process.managed_process,
        Some(managed)
    );
    assert_eq!(correlator.calls().len(), 1);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accessibility_revision_change_fences_an_inflight_process_reply()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ControlledPidCorrelator::new(Ok(vec![leader(101, managed)])));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();
    let candidate = service
        .accessibility_correlation_snapshot_blocking(std::time::Duration::from_millis(250))?;

    let query_service = Arc::clone(&service);
    let query = tokio::spawn(async move {
        query_service
            .query_for_principal(
                "alice".to_owned(),
                managed_query(desktop_id, generation, managed),
            )
            .await
    });
    correlator.wait_until_called().await;
    service
        .replace_accessibility_correlations(
            candidate.revision,
            vec![candidate.windows[0].window.clone()],
        )
        .await?;
    correlator.release();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), query).await???;
    assert_eq!(result.windows.len(), 1);
    assert_eq!(
        result.windows[0].snapshot.process.managed_process,
        Some(managed)
    );
    assert_eq!(correlator.calls.load(Ordering::Acquire), 2);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn accessibility_mutation_invalidates_a_completed_process_refresh_cache()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([
        Ok(vec![leader(101, managed)]),
        Ok(vec![leader(101, managed)]),
    ]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let first = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    assert_eq!(first.windows.len(), 1);
    let candidate = service
        .accessibility_correlation_snapshot_blocking(std::time::Duration::from_millis(250))?;
    service
        .replace_accessibility_correlations(
            candidate.revision,
            vec![candidate.windows[0].window.clone()],
        )
        .await?;
    let second = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    assert_eq!(second.windows.len(), 1);
    assert_eq!(correlator.calls().len(), 2);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_refresh_hit_rechecks_sequence_after_a_forced_toctou_interleaving()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([
        Ok(vec![leader(101, managed)]),
        Ok(vec![leader(101, managed)]),
    ]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();
    service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;

    let hook = Arc::new(ProcessRefreshCacheHitHook::new());
    *service
        .process_refresh
        .cache_hit_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&hook));
    let observed = hook.observed.notified();
    tokio::pin!(observed);
    observed.as_mut().enable();
    let query_service = Arc::clone(&service);
    let query = tokio::spawn(async move {
        query_service
            .query_for_principal(
                "bob".to_owned(),
                managed_query(desktop_id, generation, managed),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), observed).await?;

    let candidate = service
        .accessibility_correlation_snapshot_blocking(std::time::Duration::from_millis(250))?;
    service
        .replace_accessibility_correlations(
            candidate.revision,
            vec![candidate.windows[0].window.clone()],
        )
        .await?;
    hook.release();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), query).await???;
    assert_eq!(result.windows.len(), 1);
    assert_eq!(correlator.calls().len(), 2);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_refresh_results_stay_low_across_all_production_response_shapes()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let replies = std::iter::repeat_with(|| Ok(vec![leader(101, managed)])).take(8);
    let correlator = Arc::new(ScriptedPidCorrelator::new(replies));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator,
    )?;
    service.enable_process_lifecycle_authority();
    let initial = service
        .query_for_principal(
            "initial".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    let window = initial.windows[0].snapshot.window.clone();

    let list_service = Arc::clone(&service);
    let list = race_unavailable_refresh_with_recommit(
        &service,
        desktop_id,
        generation,
        managed,
        async move {
            list_service
                .list_for_principal(
                    "list".to_owned(),
                    WindowListRequest {
                        desktop_id,
                        desktop_generation: generation,
                        limit: MAX_WINDOW_PAGE_LIMIT,
                        order: WindowOrder::XidAscending,
                        cursor: None,
                    },
                )
                .await
        },
    )
    .await?;
    assert_low(&list.windows[0], 101);

    let snapshot_service = Arc::clone(&service);
    let snapshot_window = window.clone();
    let snapshot = race_unavailable_refresh_with_recommit(
        &service,
        desktop_id,
        generation,
        managed,
        async move {
            snapshot_service
                .snapshot_for_principal(
                    "snapshot".to_owned(),
                    WindowSnapshotRequest {
                        desktop_id,
                        desktop_generation: generation,
                        target: WindowSnapshotTarget::Reference {
                            window: snapshot_window,
                        },
                    },
                )
                .await
        },
    )
    .await?;
    assert_low(&snapshot.window, 101);

    let query_service = Arc::clone(&service);
    let query = race_unavailable_refresh_with_recommit(
        &service,
        desktop_id,
        generation,
        managed,
        async move {
            query_service
                .query_for_principal(
                    "query".to_owned(),
                    WindowQueryRequest {
                        desktop_id,
                        desktop_generation: generation,
                        selector: title_selector(),
                        order: WindowOrder::XidAscending,
                        limit: MAX_WINDOW_PAGE_LIMIT,
                        cursor: None,
                    },
                )
                .await
        },
    )
    .await?;
    assert_low(&query.windows[0], 101);

    let resolve_service = Arc::clone(&service);
    let resolve = race_unavailable_refresh_with_recommit(
        &service,
        desktop_id,
        generation,
        managed,
        async move {
            resolve_service
                .resolve_for_principal(
                    "resolve".to_owned(),
                    WindowResolveRequest {
                        desktop_id,
                        desktop_generation: generation,
                        selector: title_selector(),
                        order: WindowOrder::XidAscending,
                        match_policy: WindowSingleMatchPolicy::ExactlyOne,
                    },
                )
                .await
        },
    )
    .await?;
    assert_low(&resolve.window, 101);

    let accessibility_service = Arc::clone(&service);
    let accessibility = race_unavailable_refresh_with_recommit(
        &service,
        desktop_id,
        generation,
        managed,
        async move {
            accessibility_service
                .accessibility_correlation_snapshot()
                .await
        },
    )
    .await?;
    assert_eq!(accessibility.windows.len(), 1);
    assert_eq!(accessibility.windows[0].process.reported_pid, Some(101));
    assert_eq!(
        accessibility.windows[0].process.managed_process, None,
        "async accessibility must not leak the later high-confidence recommit"
    );
    assert_eq!(
        accessibility.windows[0].process.confidence,
        WindowProcessConfidence::Low
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_control_final_snapshot_scrubs_process_authority_disabled_after_effect()
-> Result<(), Box<dyn Error>> {
    use crate::control_plane::{WindowControlBackendError, WindowControlRuntime};
    use xenoteer_protocol::{
        Command, WindowCloseCommand, WindowCloseWaitPolicy, WindowControlResult,
    };
    use xenoteer_x11::{
        RawWindowControlEvidence, RawWindowControlObservation, RawWindowControlOutcome,
    };

    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator,
    )?;
    service.enable_process_lifecycle_authority();
    let initial = service
        .query_for_principal(
            "initial".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    let window = initial.windows[0].snapshot.window.clone();
    assert_eq!(
        initial.windows[0].snapshot.process.managed_process,
        Some(managed)
    );

    let handler_service = Arc::clone(&service);
    let handler = Arc::new(
        move |request, revalidate: crate::control_plane::WindowControlRevalidator, _| {
            revalidate().map_err(|_| WindowControlBackendError::ReplyUnavailable)?;
            handler_service.disable_process_lifecycle_authority();
            Ok(RawWindowControlEvidence {
                requested: request,
                outcome: RawWindowControlOutcome::TimedOut,
                observed: RawWindowControlObservation::Close {
                    exists: true,
                    viewable: Some(true),
                },
                capabilities: None,
                warnings: Vec::new(),
            })
        },
    );
    let runtime = WindowControlRuntime::new_scripted(Arc::clone(&service), handler);
    let Some(WindowControlResult::Closed(result)) =
        runtime.execute_window_control_for_test(Command::WindowClose(WindowCloseCommand {
            window,
            wait_for: WindowCloseWaitPolicy::UnmappedOrDestroyed,
        }))
    else {
        return Err("window close did not produce a successful control result".into());
    };
    let final_snapshot = result
        .final_snapshot
        .as_ref()
        .ok_or("timed-out close omitted its final snapshot")?;
    assert_eq!(final_snapshot.process.reported_pid, Some(101));
    assert_eq!(final_snapshot.process.managed_process, None);
    assert_eq!(
        final_snapshot.process.confidence,
        WindowProcessConfidence::Low
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn relayed_process_exit_invalidates_stale_managed_process_selector_truth()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([
        Ok(vec![leader(101, managed)]),
        Ok(vec![no_match(101)]),
    ]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let before = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    assert_eq!(before.windows.len(), 1);

    let exit: xenoteer_processd::BrokerProcessEvent = serde_json::from_value(serde_json::json!({
        "sequence": 1,
        "principal_id": "alice",
        "payload": {
            "application": "fixture",
            "process": {
                "process": managed,
                "state": "exited",
                "exit": {
                    "code": 0,
                    "signal": null,
                    "core_dumped": false
                }
            },
            "termination_requested": false,
            "forced_escalation": false
        }
    }))?;
    crate::control_plane::relay_process_event_for_observation_test(
        Arc::clone(&service),
        generation,
        exit,
    )
    .await?;

    let after = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await?;
    assert!(after.windows.is_empty());
    assert_eq!(correlator.calls().len(), 2);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_managed_process_wait_wakes_after_late_window_correlation()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let backend_state = Arc::new(Mutex::new(MutableWindowBackendState::default()));
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(MutableWindowBackend {
            state: Arc::clone(&backend_state),
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let waiting_service = Arc::clone(&service);
    let waiter = tokio::spawn(async move {
        waiting_service
            .wait_for_principal(
                "alice".to_owned(),
                managed_wait(desktop_id, generation, managed),
            )
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    {
        let mut state = backend_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .windows
            .insert(42, titled_raw(42, 101, "Managed editor")?);
        state.events.push_back(ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::ObserveWindow { window: 42 },
        });
    }
    service.control.notify();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .map_err(|_| "managed-process wait did not wake")?
        .map_err(|_| "managed-process wait task panicked")??;
    assert_eq!(result.status, WindowWaitStatus::Matched);
    assert_eq!(result.windows.len(), 1);
    assert_eq!(
        result.windows[0].snapshot.process.managed_process,
        Some(managed)
    );
    let change_sequence = service.process_refresh.model_changed.sequence();
    let refreshed_sequence = service
        .process_refresh
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .refreshed_change_sequence;
    assert_eq!(
        correlator.calls(),
        vec![(generation, vec![101])],
        "change_sequence={change_sequence}, refreshed_sequence={refreshed_sequence:?}"
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn title_only_late_window_waiters_share_refresh_and_return_committed_process()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let backend_state = Arc::new(Mutex::new(MutableWindowBackendState::default()));
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(MutableWindowBackend {
            state: Arc::clone(&backend_state),
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let first_service = Arc::clone(&service);
    let first = tokio::spawn(async move {
        first_service
            .wait_for_principal("alice".to_owned(), title_wait(desktop_id, generation))
            .await
    });
    let second_service = Arc::clone(&service);
    let second = tokio::spawn(async move {
        second_service
            .wait_for_principal("bob".to_owned(), title_wait(desktop_id, generation))
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    {
        let mut state = backend_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .windows
            .insert(42, titled_raw(42, 101, "Managed editor")?);
        state.events.push_back(ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::ObserveWindow { window: 42 },
        });
    }
    service.control.notify();

    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::try_join!(first, second)
    })
    .await
    .map_err(|_| "title-only waits did not wake")?
    .map_err(|_| "title-only wait task panicked")?;
    for result in [first?, second?] {
        assert_eq!(result.status, WindowWaitStatus::Matched);
        assert_eq!(result.windows.len(), 1);
        assert_eq!(
            result.windows[0].snapshot.process.managed_process,
            Some(managed)
        );
    }
    let change_sequence = service.process_refresh.model_changed.sequence();
    let refreshed_sequence = service
        .process_refresh
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .refreshed_change_sequence;
    assert_eq!(
        correlator.calls(),
        vec![(generation, vec![101])],
        "change_sequence={change_sequence}, refreshed_sequence={refreshed_sequence:?}"
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn processd_outage_keeps_title_wait_and_accessibility_observation_low_confidence()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let correlator = Arc::new(ScriptedPidCorrelator::new([
        Err(PidCorrelationError),
        Err(PidCorrelationError),
        Err(PidCorrelationError),
    ]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator,
    )?;
    service.enable_process_lifecycle_authority();

    let waited = service
        .wait_for_principal("alice".to_owned(), title_wait(desktop_id, generation))
        .await?;
    assert_eq!(waited.status, WindowWaitStatus::Matched);
    assert_eq!(waited.windows[0].snapshot.process.managed_process, None);
    assert_eq!(
        waited.windows[0].snapshot.process.confidence,
        WindowProcessConfidence::Low
    );
    let accessibility = service.accessibility_correlation_snapshot().await?;
    assert_eq!(accessibility.windows[0].process.managed_process, None);
    assert_eq!(
        accessibility.windows[0].process.confidence,
        WindowProcessConfidence::Low
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn managed_process_query_fails_closed_when_processd_is_unavailable()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([
        Err(PidCorrelationError),
        Err(PidCorrelationError),
        Err(PidCorrelationError),
    ]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let result = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await;
    assert!(matches!(
        result,
        Err(ControlPlaneError::CapabilityUnavailable)
    ));
    assert_eq!(correlator.calls().len(), 1);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn lifecycle_authority_defaults_closed_and_keeps_ordinary_observation_low()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;

    let managed_result = service
        .query_for_principal(
            "alice".to_owned(),
            managed_query(desktop_id, generation, managed),
        )
        .await;
    assert!(matches!(
        managed_result,
        Err(ControlPlaneError::CapabilityUnavailable)
    ));
    let ordinary = service
        .list_for_principal(
            "alice".to_owned(),
            WindowListRequest {
                desktop_id,
                desktop_generation: generation,
                limit: MAX_WINDOW_PAGE_LIMIT,
                order: WindowOrder::XidAscending,
                cursor: None,
            },
        )
        .await?;
    assert_low(&ordinary.windows[0], 101);
    assert!(correlator.calls().is_empty());

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_lifecycle_fence_rejects_an_inflight_initial_correlation_reply()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ControlledPidCorrelator::new(Ok(vec![leader(101, managed)])));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let query_service = Arc::clone(&service);
    let query = tokio::spawn(async move {
        query_service
            .query_for_principal(
                "alice".to_owned(),
                managed_query(desktop_id, generation, managed),
            )
            .await
    });
    correlator.wait_until_called().await;
    service.invalidate_process_correlations().await?;
    correlator.release();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), query).await??;
    assert!(matches!(
        result,
        Err(ControlPlaneError::CapabilityUnavailable)
    ));
    let ordinary = service
        .list_for_principal(
            "alice".to_owned(),
            WindowListRequest {
                desktop_id,
                desktop_generation: generation,
                limit: MAX_WINDOW_PAGE_LIMIT,
                order: WindowOrder::XidAscending,
                cursor: None,
            },
        )
        .await?;
    assert_low(&ordinary.windows[0], 101);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_managed_queries_share_one_failed_correlation_flight()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ControlledPidCorrelator::new(Err(PidCorrelationError)));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let mut queries = Vec::new();
    for principal in 0..8 {
        let service = Arc::clone(&service);
        queries.push(tokio::spawn(async move {
            service
                .query_for_principal(
                    format!("principal-{principal}"),
                    managed_query(desktop_id, generation, managed),
                )
                .await
        }));
    }
    correlator.wait_until_called().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    correlator.release();
    for query in queries {
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(10), query).await??,
            Err(ControlPlaneError::CapabilityUnavailable)
        ));
    }
    assert_eq!(correlator.calls.load(Ordering::Acquire), 1);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn healthy_managed_process_wait_returns_typed_timeout() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let actual = process(generation, 101);
    let absent = process(generation, 202);
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, actual)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator,
    )?;
    service.enable_process_lifecycle_authority();
    let mut request = managed_wait(desktop_id, generation, absent);
    request.timeout_ms = 20;

    let result = service
        .wait_for_principal("alice".to_owned(), request)
        .await?;
    assert_eq!(result.status, WindowWaitStatus::TimedOut);
    assert!(!result.predicate_satisfied);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn short_wait_deadline_is_not_extended_by_slow_process_correlation()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let correlator = Arc::new(PendingPidCorrelator::default());
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: titled_raw(42, 101, "Managed editor")?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator,
    )?;
    service.enable_process_lifecycle_authority();
    let mut request = title_wait(desktop_id, generation);
    request.timeout_ms = 10;
    request.target = WindowWaitTarget::Selector {
        selector: WindowSelector::Predicate {
            predicate: WindowPredicate::Text {
                field: WindowTextField::Title,
                matcher: WindowStringMatch::Exact {
                    value: "not present".to_owned(),
                    case_sensitive: true,
                },
            },
        },
        quantifier: WindowWaitSelectorQuantifier::Any,
    };
    let started = std::time::Instant::now();

    let result = service
        .wait_for_principal("alice".to_owned(), request)
        .await?;
    assert_eq!(result.status, WindowWaitStatus::TimedOut);
    assert!(started.elapsed() < std::time::Duration::from_millis(200));

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_stale_match_rewaits_for_a_later_real_match_before_same_deadline()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let backend_state = Arc::new(Mutex::new(MutableWindowBackendState {
        windows: BTreeMap::from([(42, titled_raw(42, 101, "Managed editor")?)]),
        events: VecDeque::new(),
    }));
    let correlator = Arc::new(ControlledPidCorrelator::new(Ok(vec![leader(101, managed)])));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(MutableWindowBackend {
            state: Arc::clone(&backend_state),
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();
    let mut request = title_wait(desktop_id, generation);
    request.timeout_ms = 500;

    let wait_service = Arc::clone(&service);
    let waiter = tokio::spawn(async move {
        wait_service
            .wait_for_principal("alice".to_owned(), request)
            .await
    });
    correlator.wait_until_called().await;
    {
        let mut backend = backend_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backend
            .windows
            .insert(42, titled_raw(42, 101, "temporarily absent")?);
        backend.events.push_back(ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::ObserveWindow { window: 42 },
        });
    }
    service.control.notify();
    correlator.release();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        !waiter.is_finished(),
        "stale early match terminalized instead of re-registering"
    );
    {
        let mut backend = backend_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backend
            .windows
            .insert(42, titled_raw(42, 101, "Managed editor")?);
        backend.events.push_back(ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::ObserveWindow { window: 42 },
        });
    }
    service.control.notify();

    let result = tokio::time::timeout(std::time::Duration::from_millis(500), waiter).await???;
    assert_eq!(result.status, WindowWaitStatus::Matched);
    assert_eq!(
        result.windows[0]
            .snapshot
            .metadata
            .title
            .as_ref()
            .map(|title| title.value.as_str()),
        Some("Managed editor")
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_window_reconcile_crossing_wait_deadline_cannot_publish_a_late_match()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let backend_state = Arc::new(Mutex::new(MutableWindowBackendState {
        windows: BTreeMap::from([
            (42, titled_raw(42, 101, "not yet")?),
            (43, titled_raw(43, 102, "also absent")?),
        ]),
        events: VecDeque::new(),
    }));
    let delay_ms = Arc::new(AtomicU64::new(0));
    let settings = ObservationServiceSettings::new(
        8,
        2,
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
        std::time::Duration::from_millis(500),
        std::time::Duration::from_secs(1),
        std::time::Duration::from_millis(1),
    )?;
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(DelayedMutableWindowBackend {
            state: Arc::clone(&backend_state),
            delay_ms: Arc::clone(&delay_ms),
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        settings,
        Arc::new(ScriptedPidCorrelator::new([])),
    )?;

    let mut request = title_wait(desktop_id, generation);
    request.timeout_ms = 25;
    let started = std::time::Instant::now();
    let wait_service = Arc::clone(&service);
    let waiter = tokio::spawn(async move {
        wait_service
            .wait_for_principal("alice".to_owned(), request)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(!waiter.is_finished());

    {
        let mut backend = backend_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backend
            .windows
            .insert(42, titled_raw(42, 101, "Managed editor")?);
        backend.events.push_back(ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::RebuildInventory,
        });
    }
    delay_ms.store(120, Ordering::Release);
    service.control.notify();

    let result = tokio::time::timeout(std::time::Duration::from_millis(800), waiter).await???;
    assert_eq!(result.status, WindowWaitStatus::TimedOut);
    assert!(!result.predicate_satisfied);
    assert_eq!(
        result.matched_count, 1,
        "the post-cutoff model would match, so TimedOut proves it did not terminalize the wait"
    );
    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
    assert!(started.elapsed() < std::time::Duration::from_millis(800));

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[test]
fn repeated_unavailability_preserves_ordinary_list_and_query_cursors() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let mut state = ModelState::new(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let first = raw(41, 101)?;
    let second = raw(42, 202)?;
    state.reconcile_raw(
        &RootInventory {
            windows: vec![41, 42],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        },
        &[first, second],
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    state.invalidate_process_correlations(MonotonicMillis::new(2))?;
    let list_first = state.list(
        "alice",
        &WindowListRequest {
            desktop_id,
            desktop_generation: generation,
            limit: 1,
            order: WindowOrder::XidAscending,
            cursor: None,
        },
        MonotonicMillis::new(2),
    )?;
    let list_cursor = list_first.next_cursor.ok_or("missing list cursor")?;
    state.invalidate_process_correlations(MonotonicMillis::new(3))?;
    let list_second = state.list(
        "alice",
        &WindowListRequest {
            desktop_id,
            desktop_generation: generation,
            limit: 1,
            order: WindowOrder::XidAscending,
            cursor: Some(list_cursor),
        },
        MonotonicMillis::new(3),
    )?;
    assert_eq!(list_second.windows.len(), 1);

    let selector = WindowSelector::Predicate {
        predicate: WindowPredicate::MapState {
            value: WindowMapState::Viewable,
        },
    };
    let query_first = state.query(
        "alice",
        &WindowQueryRequest {
            desktop_id,
            desktop_generation: generation,
            selector: selector.clone(),
            order: WindowOrder::XidAscending,
            limit: 1,
            cursor: None,
        },
        MonotonicMillis::new(3),
    )?;
    let query_cursor = query_first.next_cursor.ok_or("missing query cursor")?;
    state.invalidate_process_correlations(MonotonicMillis::new(4))?;
    let query_second = state.query(
        "alice",
        &WindowQueryRequest {
            desktop_id,
            desktop_generation: generation,
            selector,
            order: WindowOrder::XidAscending,
            limit: 1,
            cursor: Some(query_cursor),
        },
        MonotonicMillis::new(4),
    )?;
    assert_eq!(query_second.windows.len(), 1);
    Ok(())
}

#[test]
fn managed_process_query_cursor_is_rejected_after_correlation_truth_changes()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let mut state = ModelState::new(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let first = raw(41, 101)?;
    let second = raw(42, 101)?;
    state.reconcile_raw(
        &RootInventory {
            windows: vec![41, 42],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        },
        &[first, second],
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let snapshot = state.correlation_snapshot(MonotonicMillis::new(2))?;
    let assignments = snapshot
        .windows
        .iter()
        .map(|window| ProcessCorrelationAssignment {
            window: window.window.clone(),
            reported_pid: window.process.reported_pid,
            upgrade: Some(ProcessCorrelationUpgrade {
                process: managed,
                evidence: WindowProcessEvidence::ProcStartTime,
            }),
        })
        .collect::<Vec<_>>();
    state.replace_process_correlations(
        snapshot.revision,
        snapshot.correlation_epoch,
        generation,
        &assignments,
        MonotonicMillis::new(2),
    )?;

    let selector = WindowSelector::Predicate {
        predicate: WindowPredicate::ManagedProcess { process: managed },
    };
    let first_page = state.query(
        "alice",
        &WindowQueryRequest {
            desktop_id,
            desktop_generation: generation,
            selector: selector.clone(),
            order: WindowOrder::XidAscending,
            limit: 1,
            cursor: None,
        },
        MonotonicMillis::new(3),
    )?;
    assert_eq!(first_page.windows.len(), 1);
    let cursor = first_page
        .next_cursor
        .ok_or("missing managed-process query cursor")?;

    state.invalidate_process_correlations(MonotonicMillis::new(4))?;
    let continuation = state.query(
        "alice",
        &WindowQueryRequest {
            desktop_id,
            desktop_generation: generation,
            selector,
            order: WindowOrder::XidAscending,
            limit: 1,
            cursor: Some(cursor),
        },
        MonotonicMillis::new(4),
    );
    assert!(matches!(continuation, Err(ControlPlaneError::NotFound)));
    Ok(())
}

#[test]
fn nested_managed_process_selectors_are_detected_across_all_boolean_nodes() {
    let process = process(DesktopGeneration::new(), 101);
    let leaf = WindowSelector::Predicate {
        predicate: WindowPredicate::ManagedProcess { process },
    };
    assert!(selector_requires_managed_process(&leaf));
    assert!(selector_requires_managed_process(&WindowSelector::All {
        selectors: vec![
            WindowSelector::Predicate {
                predicate: WindowPredicate::Active { value: true },
            },
            leaf.clone()
        ],
    }));
    assert!(selector_requires_managed_process(&WindowSelector::Any {
        selectors: vec![
            WindowSelector::Predicate {
                predicate: WindowPredicate::Focused { value: true },
            },
            leaf.clone()
        ],
    }));
    assert!(selector_requires_managed_process(&WindowSelector::Not {
        selector: Box::new(leaf),
    }));
}

#[tokio::test]
async fn model_change_sequence_closes_change_before_notify_registration_window()
-> Result<(), Box<dyn Error>> {
    let changes = ProcessCorrelationModelChanges::new();
    let observed = changes.sequence();
    changes.publish();
    let notified = changes.notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();

    assert_ne!(changes.sequence(), observed);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), notified)
            .await
            .is_err(),
        "the sequence, not a non-sticky notification, must detect this interleaving"
    );
    Ok(())
}

#[tokio::test]
async fn leader_and_process_group_matches_upgrade_exact_process_evidence()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let leader_process = process(generation, 101);
    let group_process = process(generation, 900);
    let correlator = ScriptedPidCorrelator::new([Ok(vec![
        leader(101, leader_process),
        group(202, group_process),
    ])]);
    let mut entries = vec![
        entry(desktop_id, generation, 1, 101)?,
        entry(desktop_id, generation, 2, 202)?,
    ];

    enrich_entries(&correlator, generation, &mut entries).await;

    assert_eq!(
        entries[0].snapshot.process.managed_process,
        Some(leader_process)
    );
    assert_eq!(
        entries[0].snapshot.process.evidence,
        vec![
            WindowProcessEvidence::NetWmPid,
            WindowProcessEvidence::ProcStartTime,
        ]
    );
    assert_eq!(
        entries[1].snapshot.process.managed_process,
        Some(group_process)
    );
    assert_eq!(
        entries[1].snapshot.process.evidence,
        vec![
            WindowProcessEvidence::NetWmPid,
            WindowProcessEvidence::ProcessGroup,
        ]
    );
    for entry in &entries {
        assert_eq!(
            entry.snapshot.process.confidence,
            WindowProcessConfidence::High
        );
        assert!(!entry.snapshot.process.conflict);
        entry.validate()?;
    }
    Ok(())
}

#[tokio::test]
async fn no_match_and_broker_failure_preserve_low_net_wm_pid_evidence() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let correlator =
        ScriptedPidCorrelator::new([Ok(vec![no_match(101)]), Err(PidCorrelationError)]);
    let mut no_match_entry = vec![entry(desktop_id, generation, 1, 101)?];
    enrich_entries(&correlator, generation, &mut no_match_entry).await;
    assert_low(&no_match_entry[0], 101);

    let mut unavailable_entry = vec![entry(desktop_id, generation, 2, 202)?];
    enrich_entries(&correlator, generation, &mut unavailable_entry).await;
    assert_low(&unavailable_entry[0], 202);
    Ok(())
}

#[tokio::test]
async fn malformed_partial_duplicate_and_order_mismatched_replies_fail_open()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let pids = [101, 202];
    let malformed = ProcessRef {
        proc_start_ticks: 0,
        ..process(generation, 101)
    };
    let cases = [
        vec![leader(101, malformed), no_match(202)],
        vec![leader(101, process(generation, 101))],
        vec![
            leader(101, process(generation, 101)),
            leader(101, process(generation, 101)),
        ],
        vec![no_match(202), no_match(101)],
        vec![no_match(303), no_match(202)],
    ];

    for reply in cases {
        let correlator = ScriptedPidCorrelator::new([Ok(reply)]);
        let mut entries = vec![
            entry(desktop_id, generation, 1, pids[0])?,
            entry(desktop_id, generation, 2, pids[1])?,
        ];
        enrich_entries(&correlator, generation, &mut entries).await;
        assert_low(&entries[0], pids[0]);
        assert_low(&entries[1], pids[1]);
    }
    Ok(())
}

#[tokio::test]
async fn wrong_generation_and_nonexact_leader_pid_fail_open() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let cases = [
        leader(101, process(DesktopGeneration::new(), 101)),
        leader(101, process(generation, 999)),
    ];
    for reply in cases {
        let correlator = ScriptedPidCorrelator::new([Ok(vec![reply])]);
        let mut entries = vec![entry(desktop_id, generation, 1, 101)?];
        enrich_entries(&correlator, generation, &mut entries).await;
        assert_low(&entries[0], 101);
    }
    Ok(())
}

#[tokio::test]
async fn unique_nonzero_pids_are_batched_at_the_processd_limit() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let total = u32::try_from(MAX_PROCESS_CORRELATION_PIDS + 2)?;
    let mut entries = (1..=total)
        .map(|pid| entry(desktop_id, generation, pid, pid))
        .collect::<Result<Vec<_>, _>>()?;
    entries.push(entry(desktop_id, generation, total + 1, 1)?);
    let first = (1..=u32::try_from(MAX_PROCESS_CORRELATION_PIDS)?)
        .map(no_match)
        .collect::<Vec<_>>();
    let second = ((u32::try_from(MAX_PROCESS_CORRELATION_PIDS)? + 1)..=total)
        .map(no_match)
        .collect::<Vec<_>>();
    let correlator = ScriptedPidCorrelator::new([Ok(first), Ok(second)]);

    enrich_entries(&correlator, generation, &mut entries).await;

    let calls = correlator.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, generation);
    assert_eq!(calls[0].1.len(), MAX_PROCESS_CORRELATION_PIDS);
    assert_eq!(calls[1].1.len(), 2);
    assert_eq!(calls[0].1[0], 1);
    assert_eq!(calls[1].1, vec![total - 1, total]);

    let failing = ScriptedPidCorrelator::new([
        Err(PidCorrelationError),
        Ok(((u32::try_from(MAX_PROCESS_CORRELATION_PIDS)? + 1)..=total)
            .map(no_match)
            .collect()),
    ]);
    let mut fail_open_entries = (1..=total)
        .map(|pid| entry(desktop_id, generation, pid + 100, pid))
        .collect::<Result<Vec<_>, _>>()?;
    enrich_entries(&failing, generation, &mut fail_open_entries).await;
    assert_eq!(failing.calls().len(), 1);
    for (index, entry) in fail_open_entries.iter().enumerate() {
        assert_low(entry, u32::try_from(index)? + 1);
    }
    Ok(())
}

#[tokio::test]
async fn mixed_matches_and_no_matches_continue_across_correlation_batches()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let maximum = u32::try_from(MAX_PROCESS_CORRELATION_PIDS)?;
    let total = maximum + 2;
    let mut entries = (1..=total)
        .map(|pid| entry(desktop_id, generation, pid, pid))
        .collect::<Result<Vec<_>, _>>()?;

    let first_process = process(generation, 1);
    let last_process = process(generation, total);
    let mut first_reply = (1..=maximum).map(no_match).collect::<Vec<_>>();
    first_reply[0] = leader(1, first_process);
    let correlator = ScriptedPidCorrelator::new([
        Ok(first_reply),
        Ok(vec![no_match(maximum + 1), leader(total, last_process)]),
    ]);

    enrich_entries(&correlator, generation, &mut entries).await;

    assert_eq!(
        entries[0].snapshot.process.managed_process,
        Some(first_process)
    );
    for (index, entry) in entries[1..entries.len() - 1].iter().enumerate() {
        assert_low(entry, u32::try_from(index)? + 2);
    }
    assert_eq!(
        entries
            .last()
            .and_then(|entry| entry.snapshot.process.managed_process),
        Some(last_process)
    );
    assert_eq!(correlator.calls().len(), 2);
    Ok(())
}

#[tokio::test]
async fn pending_broker_is_bounded_by_one_total_monotonic_deadline() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let correlator = PendingPidCorrelator::default();
    let total = u32::try_from(MAX_PROCESS_CORRELATION_PIDS + 1)?;
    let mut entries = (1..=total)
        .map(|pid| entry(desktop_id, generation, pid, pid))
        .collect::<Result<Vec<_>, _>>()?;

    tokio::time::timeout(
        PROCESS_CORRELATION_TOTAL_TIMEOUT * 4,
        enrich_entries(&correlator, generation, &mut entries),
    )
    .await
    .map_err(|_| "correlation exceeded its total response budget")?;

    assert_eq!(
        correlator
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    for (index, entry) in entries.iter().enumerate() {
        assert_low(entry, u32::try_from(index)? + 1);
    }
    Ok(())
}

#[tokio::test]
async fn every_public_observation_response_shape_is_enriched_and_revalidated()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let replies = (101..=105)
        .map(|pid| Ok(vec![leader(pid, process(generation, pid))]))
        .collect::<Vec<_>>();
    let correlator = ScriptedPidCorrelator::new(replies);
    let revision = WindowModelRevision::new(1)?;

    let list = enrich_list_result(
        &correlator,
        desktop_id,
        generation,
        WindowListPage {
            desktop_id,
            desktop_generation: generation,
            snapshot_revision: revision,
            windows: vec![entry(desktop_id, generation, 1, 101)?],
            next_cursor: None,
        },
    )
    .await
    .map_err(|_| "list enrichment failed")?;
    let snapshot = enrich_snapshot_result(
        &correlator,
        desktop_id,
        generation,
        WindowSnapshotResult {
            snapshot_revision: revision,
            window: entry(desktop_id, generation, 2, 102)?,
        },
    )
    .await
    .map_err(|_| "snapshot enrichment failed")?;
    let query = enrich_query_result(
        &correlator,
        desktop_id,
        generation,
        WindowQueryPage {
            desktop_id,
            desktop_generation: generation,
            snapshot_revision: revision,
            windows: vec![entry(desktop_id, generation, 3, 103)?],
            next_cursor: None,
        },
    )
    .await
    .map_err(|_| "query enrichment failed")?;
    let resolve = enrich_resolve_result(
        &correlator,
        desktop_id,
        generation,
        WindowResolveResult {
            desktop_id,
            desktop_generation: generation,
            snapshot_revision: revision,
            window: entry(desktop_id, generation, 4, 104)?,
        },
    )
    .await
    .map_err(|_| "resolve enrichment failed")?;
    let wait = enrich_wait_result(
        &correlator,
        desktop_id,
        generation,
        WindowWaitResult {
            desktop_id,
            desktop_generation: generation,
            status: WindowWaitStatus::Matched,
            evaluated_revision: revision,
            predicate_satisfied: true,
            matched_count: 1,
            windows: vec![entry(desktop_id, generation, 5, 105)?],
        },
    )
    .await
    .map_err(|_| "wait enrichment failed")?;

    assert_eq!(
        list.windows[0].snapshot.process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(
        snapshot.window.snapshot.process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(
        query.windows[0].snapshot.process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(
        resolve.window.snapshot.process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(
        wait.windows[0].snapshot.process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(correlator.calls().len(), 5);
    Ok(())
}

#[tokio::test]
async fn async_internal_correlation_snapshot_commits_but_blocking_queue_head_stays_conservative()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let managed = process(generation, 101);
    let correlator = Arc::new(ScriptedPidCorrelator::new([Ok(vec![leader(101, managed)])]));
    let (service, shutdown, join) = spawn_model_actor_with_correlator(
        Box::new(SingleWindowBackend {
            snapshot: raw(42, 101)?,
        }),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
        correlator.clone(),
    )?;
    service.enable_process_lifecycle_authority();

    let enriched = service.accessibility_correlation_snapshot().await?;
    assert_eq!(enriched.windows.len(), 1);
    assert_eq!(enriched.windows[0].process.managed_process, Some(managed));
    assert_eq!(
        enriched.windows[0].process.confidence,
        WindowProcessConfidence::High
    );
    assert_eq!(correlator.calls(), vec![(generation, vec![101])]);

    let conservative_queue_head = service
        .accessibility_correlation_snapshot_blocking(std::time::Duration::from_millis(250))?;
    assert_eq!(conservative_queue_head.windows.len(), 1);
    assert_eq!(
        conservative_queue_head.windows[0].process.managed_process,
        None
    );
    assert_eq!(
        conservative_queue_head.windows[0].process.confidence,
        WindowProcessConfidence::Low
    );

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[tokio::test]
async fn invalid_response_scope_is_rejected_before_correlation() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let correlator = ScriptedPidCorrelator::new([]);
    let response = WindowListPage {
        desktop_id: DesktopId::new(),
        desktop_generation: generation,
        snapshot_revision: WindowModelRevision::new(1)?,
        windows: vec![entry(desktop_id, generation, 1, 101)?],
        next_cursor: None,
    };

    assert_eq!(
        enrich_list_result(&correlator, desktop_id, generation, response).await,
        Err(ControlPlaneError::Internal)
    );
    assert!(correlator.calls().is_empty());
    Ok(())
}
