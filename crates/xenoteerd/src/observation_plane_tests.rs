use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use xenoteer_protocol::{
    CoordinateSpace, DesktopGeneration, DesktopId, MAX_WINDOW_ATOMS, Rect, WindowFrameExtents,
    WindowIdentityHash, WindowMapState, WindowModelRevision, WindowPredicate, WindowRect,
    WindowSingleMatchPolicy, WindowStringMatch, WindowText, WindowTextField,
    WindowWaitSelectorQuantifier,
};
use xenoteer_x11::{
    FocusAncestryStatus, ObservedAtom, RootGeometryInput, RootWindowEvidenceInput,
    WindowAttributeInput, WindowPropertyInput, WindowSnapshotInput,
};

use super::*;

struct ResyncCountingSink {
    requests: AtomicU64,
}

impl ResyncCountingSink {
    const fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
        }
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

impl WindowEventSink for ResyncCountingSink {
    fn try_emit(&self, _: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        Ok(())
    }

    fn require_resync(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }
}

fn raw(window: u32) -> Result<WindowSnapshotInput, Box<dyn Error>> {
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
            reported_pid: None,
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

fn reference(window: u32) -> Result<WindowRef, Box<dyn Error>> {
    Ok(WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid: window,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("a".repeat(64))?,
    })
}

fn inventory(source: InventorySource, windows: Vec<u32>) -> RootInventory {
    RootInventory {
        windows,
        source,
        warnings: Vec::new(),
    }
}

struct ScriptedBackend {
    inventories: VecDeque<Result<RootInventory, RawBackendFailure>>,
    snapshots: VecDeque<Result<WindowSnapshotInput, RawBackendFailure>>,
    events: VecDeque<Result<ObservationActorEvent, RawBackendFailure>>,
    panic_on_event: bool,
}

struct SequentialDelayBackend {
    inventory: RootInventory,
    snapshots: VecDeque<WindowSnapshotInput>,
    delay: Duration,
}

impl ScriptedBackend {
    fn new(
        inventories: impl IntoIterator<Item = Result<RootInventory, RawBackendFailure>>,
        snapshots: impl IntoIterator<Item = Result<WindowSnapshotInput, RawBackendFailure>>,
    ) -> Self {
        Self {
            inventories: inventories.into_iter().collect(),
            snapshots: snapshots.into_iter().collect(),
            events: VecDeque::new(),
            panic_on_event: false,
        }
    }

    fn with_events(
        mut self,
        events: impl IntoIterator<Item = Result<ObservationActorEvent, RawBackendFailure>>,
    ) -> Self {
        self.events = events.into_iter().collect();
        self
    }

    fn with_event_panic(mut self) -> Self {
        self.panic_on_event = true;
        self
    }
}

impl RawObservationBackend for ScriptedBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        self.inventories
            .pop_front()
            .unwrap_or(Err(RawBackendFailure::Unavailable))
    }

    fn snapshot(
        &mut self,
        _window: u32,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        self.snapshots
            .pop_front()
            .unwrap_or(Err(RawBackendFailure::Unavailable))
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        assert!(!self.panic_on_event, "scripted observation backend panic");
        self.events.pop_front().transpose()
    }

    fn shutdown(&mut self, _timeout: Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

impl RawObservationBackend for SequentialDelayBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        std::thread::sleep(self.delay);
        Ok(self.inventory.clone())
    }

    fn snapshot(
        &mut self,
        _window: u32,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        std::thread::sleep(self.delay);
        self.snapshots
            .pop_front()
            .ok_or(RawBackendFailure::RequestFailed)
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        Ok(None)
    }

    fn shutdown(&mut self, _timeout: Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

#[derive(Default)]
struct ShutdownRaceState {
    event_selected: bool,
    release_event: bool,
}

struct ShutdownRaceBackend {
    gate: Arc<(Mutex<ShutdownRaceState>, Condvar)>,
}

impl RawObservationBackend for ShutdownRaceBackend {
    fn reconcile(
        &mut self,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<RootInventory, RawBackendFailure> {
        Ok(inventory(InventorySource::NetClientList, Vec::new()))
    }

    fn snapshot(
        &mut self,
        _window: u32,
        _control: &ModelActorControl,
        _timeout: Duration,
    ) -> Result<WindowSnapshotInput, RawBackendFailure> {
        Err(RawBackendFailure::RequestFailed)
    }

    fn try_event(&mut self) -> Result<Option<ObservationActorEvent>, RawBackendFailure> {
        let (state, wake) = &*self.gate;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.event_selected = true;
        wake.notify_all();
        while !state.release_event {
            state = wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Ok(Some(ObservationActorEvent::Failed {
            failure: xenoteer_x11::ObservationActorFailure {
                kind: ObservationActorFailureKind::BackendUnavailable,
            },
        }))
    }

    fn shutdown(&mut self, _timeout: Duration) -> ObservationActorExit {
        ObservationActorExit::Stopped
    }
}

struct ScriptedEntropy(Mutex<VecDeque<[u8; 32]>>);

impl ScriptedEntropy {
    fn new(values: impl IntoIterator<Item = [u8; 32]>) -> Self {
        Self(Mutex::new(values.into_iter().collect()))
    }
}

impl TokenEntropy for ScriptedEntropy {
    fn fill(&self, destination: &mut [u8; 32]) -> Result<(), ObservationAdapterError> {
        let Some(value) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
        else {
            return Err(ObservationAdapterError::TokenUnavailable);
        };
        *destination = value;
        Ok(())
    }
}

#[test]
fn canonical_identity_is_stable_and_malformed_vectors_are_rejected() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let input = raw(42)?;
    let first = build_identity_hash(&input, desktop_id, generation, 1)?;
    let second = build_identity_hash(&input, desktop_id, generation, 1)?;
    assert_eq!(first, second);

    let mut malformed = input;
    malformed.properties.states = vec![
        ObservedAtom {
            id: 1,
            known: Some(KnownAtom::NetWmStateHidden),
        };
        MAX_WINDOW_ATOMS + 1
    ];
    assert!(validate_raw_input(&malformed).is_err());
    assert!(WindowModelRevision::new(1).is_ok());
    Ok(())
}

#[test]
fn normalization_uses_reviewed_atom_names_and_root_state_evidence() -> Result<(), Box<dyn Error>> {
    let mut input = raw(42)?;
    input.properties.window_types = vec![ObservedAtom {
        id: 101,
        known: Some(KnownAtom::NetWmWindowTypeDialog),
    }];
    input.properties.states = vec![
        ObservedAtom {
            id: 102,
            known: Some(KnownAtom::NetWmStateHidden),
        },
        ObservedAtom {
            id: 103,
            known: Some(KnownAtom::NetWmStateModal),
        },
        ObservedAtom {
            id: 104,
            known: Some(KnownAtom::NetWmStateSticky),
        },
        ObservedAtom {
            id: 105,
            known: Some(KnownAtom::NetWmStateFocused),
        },
    ];
    input.properties.allowed_actions = vec![ObservedAtom {
        id: 106,
        known: Some(KnownAtom::NetWmActionClose),
    }];
    input.properties.protocols = vec![ObservedAtom {
        id: 107,
        known: Some(KnownAtom::WmDeleteWindow),
    }];
    input.root.active_window = Some(42);
    input.root.raw_focused_window = Some(142);
    input.root.focused_window = Some(42);
    input.root.target_contains_focus = true;
    input.root.focus_ancestry_status = FocusAncestryStatus::Resolved;
    let reference = reference(42)?;
    let snapshot = normalize_snapshot(
        &input,
        reference,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;

    assert_eq!(
        snapshot.metadata.window_types[0].as_str(),
        "_NET_WM_WINDOW_TYPE_DIALOG"
    );
    assert_eq!(snapshot.metadata.states[0].as_str(), "_NET_WM_STATE_HIDDEN");
    assert_eq!(
        snapshot.metadata.allowed_actions[0].as_str(),
        "_NET_WM_ACTION_CLOSE"
    );
    assert_eq!(snapshot.metadata.protocols[0].as_str(), "WM_DELETE_WINDOW");
    assert!(snapshot.state.hidden);
    assert!(snapshot.state.minimized);
    assert!(snapshot.state.modal);
    assert!(snapshot.state.sticky);
    assert!(snapshot.state.active);
    assert!(snapshot.state.focused);
    Ok(())
}

#[test]
fn unknown_atom_is_a_truthful_diagnostic_with_a_warning() -> Result<(), Box<dyn Error>> {
    let mut input = raw(42)?;
    input.properties.states = vec![ObservedAtom {
        id: 0xfeed_beef,
        known: None,
    }];
    let reference = reference(42)?;
    let snapshot = normalize_snapshot(
        &input,
        reference,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;

    assert_eq!(snapshot.metadata.states[0].as_str(), "0xfeedbeef");
    assert!(
        snapshot
            .warnings
            .contains(&WindowSnapshotWarning::UnsupportedPropertyEncoding {
                property: WindowAtomName::new("_NET_WM_STATE")?,
            })
    );
    Ok(())
}

#[test]
fn frame_rect_is_checked_from_root_client_geometry_and_advisory_extents()
-> Result<(), Box<dyn Error>> {
    let mut input = raw(42)?;
    input.properties.frame_extents = Some(WindowFrameExtents {
        left: 5,
        right: 7,
        top: 10,
        bottom: 12,
    });
    let snapshot = normalize_snapshot(
        &input,
        reference(42)?,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;
    let frame = snapshot
        .geometry
        .as_ref()
        .and_then(|geometry| geometry.frame_rect)
        .ok_or("missing checked frame")?;
    assert_eq!(frame.coordinate_space, CoordinateSpace::RootPhysical);
    assert_eq!(frame.rect.origin().x(), 5);
    assert_eq!(frame.rect.origin().y(), 10);
    assert_eq!(frame.rect.size()?.width(), 652);
    assert_eq!(frame.rect.size()?.height(), 502);
    assert!(
        !snapshot
            .warnings
            .contains(&WindowSnapshotWarning::FrameGeometryUnavailable)
    );
    Ok(())
}

#[test]
fn unrepresentable_frame_extents_do_not_corrupt_snapshot_geometry() -> Result<(), Box<dyn Error>> {
    let mut input = raw(42)?;
    input.geometry.client_rect = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(i32::MIN, i32::MIN, 1, 1)?,
    )?;
    input.properties.frame_extents = Some(WindowFrameExtents {
        left: 1,
        right: 0,
        top: 1,
        bottom: 0,
    });
    let snapshot = normalize_snapshot(
        &input,
        reference(42)?,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;
    assert!(
        snapshot
            .geometry
            .as_ref()
            .is_some_and(|geometry| geometry.frame_rect.is_none())
    );
    assert!(
        snapshot
            .warnings
            .contains(&WindowSnapshotWarning::FrameGeometryUnavailable)
    );
    assert!(
        snapshot
            .warnings
            .contains(&WindowSnapshotWarning::FrameExtentsUnverified)
    );
    Ok(())
}

#[test]
fn unmapped_without_hidden_evidence_is_not_labeled_minimized() -> Result<(), Box<dyn Error>> {
    let mut input = raw(42)?;
    input.attributes.map_state = WindowMapState::Unmapped;
    let snapshot = normalize_snapshot(
        &input,
        reference(42)?,
        WindowModelRevision::new(1)?,
        None,
        &BTreeMap::new(),
    )?;
    assert!(!snapshot.state.hidden);
    assert!(!snapshot.state.minimized);
    Ok(())
}

#[test]
fn only_authoritative_stacking_inventory_sets_stacking_indices() -> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientListStacking, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let (_, snapshots) = state.model.snapshot_all(MonotonicMillis::new(1))?;
    assert_eq!(snapshots[0].stacking_index, Some(0));

    state.reconcile_raw(
        &inventory(InventorySource::QueryTreeFallback, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(2),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let (_, snapshots) = state.model.snapshot_all(MonotonicMillis::new(2))?;
    assert_eq!(snapshots[0].stacking_index, None);

    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(3),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let (_, snapshots) = state.model.snapshot_all(MonotonicMillis::new(3))?;
    assert_eq!(snapshots[0].stacking_index, None);
    Ok(())
}

#[test]
fn correlation_snapshot_is_atomic_convertible_and_rejects_a_partial_universe()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let limits = WindowModelLimits {
        max_live_windows: MAX_ACCESSIBILITY_CORRELATION_CANDIDATES + 1,
        ..WindowModelLimits::default()
    };
    let mut state = ModelState::new(
        desktop_id,
        generation,
        limits,
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let mut first = raw(42)?;
    first.properties.reported_pid = Some(321);
    first.properties.title = Some(WindowText::new("Editor", false)?);
    let first_inventory = inventory(InventorySource::NetClientList, vec![42]);
    state.reconcile_raw(
        &first_inventory,
        std::slice::from_ref(&first),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;

    let correlation = state.correlation_snapshot(MonotonicMillis::new(2))?;
    assert_eq!(correlation.windows.len(), 1);
    assert_eq!(correlation.windows[0].model_revision, correlation.revision);
    let candidates = correlation.candidates()?;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].process_id, Some(321));
    assert_eq!(
        candidates[0]
            .title
            .as_ref()
            .map(NormalizedCorrelationText::as_str),
        Some("editor")
    );
    assert_eq!(
        candidates[0].top_level_extents,
        Some(first.geometry.client_rect.rect)
    );
    assert_eq!(candidates[0].observed_at, MonotonicMillis::new(2));

    let xids = (1..=MAX_ACCESSIBILITY_CORRELATION_CANDIDATES + 1)
        .map(|index| u32::try_from(index).map(|xid| xid + 1_000))
        .collect::<Result<Vec<_>, _>>()?;
    let inputs = xids
        .iter()
        .map(|xid| raw(*xid))
        .collect::<Result<Vec<_>, _>>()?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, xids),
        &inputs,
        MonotonicMillis::new(3),
        ReconcileIdentityPolicy::InvalidateAll,
    )?;
    assert!(matches!(
        state.correlation_snapshot(MonotonicMillis::new(4)),
        Err(ControlPlaneError::ResourceExhausted)
    ));
    Ok(())
}

#[test]
fn exact_occlusion_snapshot_orders_geometry_tracks_movement_and_advances_epoch()
-> Result<(), Box<dyn Error>> {
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
    let mut inputs = vec![raw(42)?, raw(43)?, raw(44)?];
    for (index, input) in inputs.iter_mut().enumerate() {
        input.geometry.client_rect = WindowRect::new(
            CoordinateSpace::RootPhysical,
            Rect::new(i32::try_from(index)? * 100, 10, 80, 80)?,
        )?;
    }
    let root = inventory(InventorySource::NetClientListStacking, vec![42, 43, 44]);
    state.reconcile_raw(
        &root,
        &inputs,
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let target = state.model.live_reference(42).cloned().ok_or("missing")?;

    let first = state.occlusion_snapshot(&target, MonotonicMillis::new(2))?;
    assert_eq!(first.stacking_epoch, 1);
    assert!(first.stacking_complete);
    assert_eq!(
        first.rectangles_above,
        vec![
            inputs[1].geometry.client_rect.rect,
            inputs[2].geometry.client_rect.rect,
        ]
    );
    assert_eq!(first.as_click_snapshot().target_window, &target);

    inputs[0].geometry.client_rect =
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(500, 500, 80, 80)?)?;
    state.reconcile_raw(
        &root,
        &inputs,
        MonotonicMillis::new(3),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let moved = state.occlusion_snapshot(&target, MonotonicMillis::new(4))?;
    assert_eq!(moved.stacking_epoch, 2);
    assert_eq!(moved.target_window, target);
    assert_eq!(
        moved.target_client_bounds,
        Some(inputs[0].geometry.client_rect.rect)
    );

    state.remove_xid(42, MonotonicMillis::new(5))?;
    assert!(matches!(
        state.occlusion_snapshot(&target, MonotonicMillis::new(6)),
        Err(ControlPlaneError::NotFound | ControlPlaneError::StaleReference { .. })
    ));
    Ok(())
}

#[test]
fn over_cap_or_unproven_stacking_is_explicitly_incomplete() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let window_count = MAX_ELEMENT_CLICK_OCCLUDERS + 2;
    let limits = WindowModelLimits {
        max_live_windows: window_count,
        ..WindowModelLimits::default()
    };
    let mut state = ModelState::new(
        desktop_id,
        generation,
        limits,
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let xids = (0..window_count)
        .map(|index| u32::try_from(index).map(|xid| xid + 1_000))
        .collect::<Result<Vec<_>, _>>()?;
    let inputs = xids
        .iter()
        .map(|xid| raw(*xid))
        .collect::<Result<Vec<_>, _>>()?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientListStacking, xids.clone()),
        &inputs,
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let target = state
        .model
        .live_reference(xids[0])
        .cloned()
        .ok_or("missing")?;
    let capped = state.occlusion_snapshot(&target, MonotonicMillis::new(2))?;
    assert_eq!(capped.rectangles_above.len(), MAX_ELEMENT_CLICK_OCCLUDERS);
    assert!(!capped.stacking_complete);

    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, xids),
        &inputs,
        MonotonicMillis::new(3),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let unproven = state.occlusion_snapshot(&target, MonotonicMillis::new(4))?;
    assert!(unproven.rectangles_above.is_empty());
    assert!(!unproven.stacking_complete);
    assert!(unproven.stacking_epoch > capped.stacking_epoch);
    Ok(())
}

#[test]
fn accessibility_correlations_are_exact_replaceable_and_survive_x11_refresh()
-> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    let root = inventory(InventorySource::NetClientList, vec![42]);
    state.reconcile_raw(
        &root,
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let window = state
        .model
        .live_reference(42)
        .cloned()
        .ok_or("missing window")?;

    let initial_revision = state.model.revision();
    let correlated_revision = state.replace_accessibility_correlations(
        initial_revision,
        std::slice::from_ref(&window),
        MonotonicMillis::new(2),
    )?;
    assert!(matches!(
        state.replace_accessibility_correlations(
            initial_revision,
            std::slice::from_ref(&window),
            MonotonicMillis::new(2),
        ),
        Err(ControlPlaneError::StaleReference { .. })
    ));
    let repeated_revision = state.replace_accessibility_correlations(
        correlated_revision,
        std::slice::from_ref(&window),
        MonotonicMillis::new(2),
    )?;
    assert_eq!(correlated_revision, repeated_revision);

    state.reconcile_raw(
        &root,
        std::slice::from_ref(&input),
        MonotonicMillis::new(3),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    state.observe_raw(&input, MonotonicMillis::new(4))?;
    let (_, refreshed) = state.model.snapshot_all(MonotonicMillis::new(4))?;
    assert!(refreshed[0].has_accessibility_application);

    let mut stale = window.clone();
    stale.observed_generation += 1;
    stale.identity_hash = WindowIdentityHash::new("b".repeat(64))?;
    let current_revision = state.model.revision();
    assert_eq!(
        state.replace_accessibility_correlations(
            current_revision,
            &[stale],
            MonotonicMillis::new(5)
        ),
        Err(ControlPlaneError::NotFound)
    );
    let (_, unchanged) = state.model.snapshot_all(MonotonicMillis::new(5))?;
    assert!(unchanged[0].has_accessibility_application);

    let current_revision = state.model.revision();
    let cleared_revision =
        state.replace_accessibility_correlations(current_revision, &[], MonotonicMillis::new(6))?;
    assert!(cleared_revision > correlated_revision);
    let (_, cleared) = state.model.snapshot_all(MonotonicMillis::new(6))?;
    assert!(!cleared[0].has_accessibility_application);
    Ok(())
}

#[test]
fn malformed_or_over_capacity_reconcile_is_rejected_before_mutation() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let limits = WindowModelLimits {
        max_live_windows: 1,
        ..WindowModelLimits::default()
    };
    let mut state = ModelState::new(
        desktop_id,
        generation,
        limits,
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
    )?;
    let initial = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&initial),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let old = state.model.live_reference(42).cloned().ok_or("missing")?;

    assert!(
        state
            .reconcile_raw(
                &inventory(InventorySource::NetClientList, vec![42, 43]),
                &[initial.clone(), raw(43)?],
                MonotonicMillis::new(2),
                ReconcileIdentityPolicy::InvalidateAll,
            )
            .is_err()
    );
    assert_eq!(state.model.live_reference(42), Some(&old));

    assert!(
        state
            .reconcile_raw(
                &inventory(InventorySource::NetClientList, vec![42]),
                &[raw(43)?],
                MonotonicMillis::new(2),
                ReconcileIdentityPolicy::InvalidateAll,
            )
            .is_err()
    );
    assert_eq!(state.model.live_reference(42), Some(&old));

    let mut malformed_focus = initial;
    malformed_focus.root.raw_focused_window = Some(99);
    assert!(validate_raw_input(&malformed_focus).is_err());
    Ok(())
}

#[test]
fn loss_resync_remints_identical_xid_birth_and_stales_the_old_reference()
-> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    let inventory = inventory(InventorySource::NetClientListStacking, vec![42]);
    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let old = state
        .model
        .live_reference(42)
        .cloned()
        .ok_or("missing first birth")?;

    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&input),
        MonotonicMillis::new(2),
        ReconcileIdentityPolicy::InvalidateAll,
    )?;
    let replacement = state
        .model
        .live_reference(42)
        .cloned()
        .ok_or("missing replacement birth")?;

    assert_ne!(old, replacement);
    assert_eq!(replacement.observed_generation, old.observed_generation + 1);
    assert!(matches!(
        state.model.resolve_exact(&old, MonotonicMillis::new(3)),
        Err(WindowModelError::StaleReference | WindowModelError::DestroyedReference)
    ));
    Ok(())
}

#[test]
fn token_registry_uses_full_entropy_retries_collisions_and_redacts_debug()
-> Result<(), Box<dyn Error>> {
    let entropy = Arc::new(ScriptedEntropy::new([[0x11; 32], [0x11; 32], [0x22; 32]]));
    let mut registry = OpaqueTokenRegistry::with_entropy(4, 50, 50, entropy)?;
    let window = reference(42)?;
    let first = registry.mint_reference("alice", window.clone(), MonotonicMillis::new(1))?;
    let second = registry.mint_reference("alice", window.clone(), MonotonicMillis::new(1))?;

    assert_eq!(first.as_str().len(), 66);
    assert_eq!(second.as_str().len(), 66);
    assert_ne!(first, second);
    assert_eq!(
        registry.resolve_reference(
            &second,
            "alice",
            window.desktop_id,
            window.desktop_generation,
            MonotonicMillis::new(1),
        )?,
        window
    );
    let debug = format!("{registry:?}");
    assert!(!debug.contains(first.as_str()));
    assert!(!debug.contains(second.as_str()));
    assert!(!debug.contains(&"11".repeat(32)));
    Ok(())
}

#[test]
fn token_registry_rejects_tamper_scope_expiry_and_collision_exhaustion()
-> Result<(), Box<dyn Error>> {
    let entropy = Arc::new(ScriptedEntropy::new(std::iter::repeat_n(
        [0x33; 32],
        MAX_TOKEN_MINT_ATTEMPTS + 1,
    )));
    let mut registry = OpaqueTokenRegistry::with_entropy(4, 10, 10, entropy)?;
    let window = reference(42)?;
    let token = registry.mint_reference("alice", window.clone(), MonotonicMillis::new(5))?;
    assert!(
        registry
            .resolve_reference(
                &token,
                "mallory",
                window.desktop_id,
                window.desktop_generation,
                MonotonicMillis::new(5),
            )
            .is_err()
    );
    let mut tampered = token.as_str().to_owned();
    tampered.replace_range(tampered.len() - 1.., "0");
    let tampered = WindowReferenceToken::new(tampered)?;
    assert!(
        registry
            .resolve_reference(
                &tampered,
                "alice",
                window.desktop_id,
                window.desktop_generation,
                MonotonicMillis::new(5),
            )
            .is_err()
    );
    assert!(
        registry
            .resolve_reference(
                &token,
                "alice",
                DesktopId::new(),
                window.desktop_generation,
                MonotonicMillis::new(5),
            )
            .is_err()
    );
    // The first live digest occupies the only scripted value. Every
    // subsequent mint attempt collides and must fail closed, never replace
    // the existing claim.
    assert!(matches!(
        registry.mint_reference("alice", window.clone(), MonotonicMillis::new(5)),
        Err(ObservationAdapterError::TokenUnavailable)
    ));
    assert!(
        registry
            .resolve_reference(
                &token,
                "alice",
                window.desktop_id,
                window.desktop_generation,
                MonotonicMillis::new(15),
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn one_failed_gap_marker_causes_exactly_one_birth_transition() -> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let sink = Arc::new(ResyncCountingSink::new());
    let mut state = ModelState::new_with_event_sink(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        1_000,
        1_000,
        sink.clone(),
    )?;
    let input = raw(42)?;
    let inventory = inventory(InventorySource::NetClientListStacking, vec![42]);
    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let first = state.model.live_reference(42).cloned().ok_or("missing")?;
    let mut backend = ScriptedBackend::new([Ok(inventory)], [Ok(input)]);
    let control = ModelActorControl::default();

    process_raw_event(
        &mut state,
        &mut backend,
        &control,
        ObservationActorEvent::Failed {
            failure: xenoteer_x11::ObservationActorFailure {
                kind: ObservationActorFailureKind::RequestFailed,
            },
        },
        &|| MonotonicMillis::new(2),
        Duration::from_millis(10),
    )?;
    assert_eq!(state.model.live_reference(42), Some(&first));
    assert_eq!(sink.requests(), 0);

    process_raw_event(
        &mut state,
        &mut backend,
        &control,
        ObservationActorEvent::ResyncRequired,
        &|| MonotonicMillis::new(3),
        Duration::from_millis(10),
    )?;
    let replacement = state.model.live_reference(42).cloned().ok_or("missing")?;
    assert_eq!(
        replacement.observed_generation,
        first.observed_generation + 1
    );
    assert_eq!(sink.requests(), 1);
    Ok(())
}

#[test]
fn stable_snapshot_failure_tombstones_the_old_birth_before_reconcile() -> Result<(), Box<dyn Error>>
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
    let input = raw(42)?;
    let inventory = inventory(InventorySource::NetClientList, vec![42]);
    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let first = state.model.live_reference(42).cloned().ok_or("missing")?;
    let mut backend = ScriptedBackend::new(
        [Ok(inventory)],
        [Err(RawBackendFailure::RequestFailed), Ok(input)],
    );

    process_raw_event(
        &mut state,
        &mut backend,
        &ModelActorControl::default(),
        ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::RefreshWindow {
                window: 42,
                refresh: xenoteer_x11::WindowRefresh::Metadata,
            },
        },
        &|| MonotonicMillis::new(2),
        Duration::from_millis(10),
    )?;

    let replacement = state.model.live_reference(42).cloned().ok_or("missing")?;
    assert_ne!(replacement, first);
    assert_eq!(
        replacement.observed_generation,
        first.observed_generation + 1
    );
    assert!(matches!(
        state.model.resolve_exact(&first, MonotonicMillis::new(2)),
        Err(WindowModelError::DestroyedReference | WindowModelError::StaleReference)
    ));
    Ok(())
}

#[test]
fn expired_resync_invalidates_immediately_and_remints_with_a_fresh_budget()
-> Result<(), Box<dyn Error>> {
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
    let original = raw(42)?;
    let inventory = inventory(InventorySource::NetClientList, vec![42]);
    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&original),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let old = state.model.live_reference(42).cloned().ok_or("missing")?;
    let replacement = raw(42)?;
    let mut backend = ScriptedBackend::new([Ok(inventory)], [Ok(replacement)]);
    let mut waiters = Vec::new();

    process_actor_event_until(
        &mut state,
        &mut waiters,
        &mut backend,
        &ModelActorControl::default(),
        ObservationActorEvent::ResyncRequired,
        &|| MonotonicMillis::new(2),
        Instant::now(),
    )?;
    assert!(state.raw_resync_pending);
    assert!(matches!(
        state.model.resolve_exact(&old, MonotonicMillis::new(2)),
        Err(WindowModelError::DestroyedReference)
    ));

    reconcile_from_backend_until(
        &mut state,
        &mut backend,
        &ModelActorControl::default(),
        &|| MonotonicMillis::new(3),
        Instant::now() + Duration::from_millis(50),
        ReconcileIdentityPolicy::InvalidateAll,
        ReconcileEventPolicy::Rebuilt(WindowModelRebuildReason::EventOverflow),
    )?;
    state.raw_resync_pending = false;
    let current = state.model.live_reference(42).cloned().ok_or("missing")?;
    assert_ne!(current, old);
    assert_eq!(current.observed_generation, old.observed_generation + 1);
    Ok(())
}

#[test]
fn snapshot_failure_reconcile_removes_only_inventory_confirmed_absence()
-> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let first = state.model.live_reference(42).cloned().ok_or("missing")?;
    let mut backend = ScriptedBackend::new(
        [Ok(inventory(InventorySource::NetClientList, Vec::new()))],
        [Err(RawBackendFailure::RequestFailed)],
    );

    process_raw_event(
        &mut state,
        &mut backend,
        &ModelActorControl::default(),
        ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::RefreshWindow {
                window: 42,
                refresh: xenoteer_x11::WindowRefresh::Metadata,
            },
        },
        &|| MonotonicMillis::new(2),
        Duration::from_millis(10),
    )?;

    assert!(state.model.live_reference(42).is_none());
    assert!(matches!(
        state.model.resolve_exact(&first, MonotonicMillis::new(3)),
        Err(WindowModelError::DestroyedReference)
    ));
    Ok(())
}

#[test]
fn reconcile_omits_only_members_that_vanish_during_snapshot() -> Result<(), Box<dyn Error>> {
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
    let first = raw(42)?;
    let vanished = raw(43)?;
    let initial = inventory(InventorySource::NetClientList, vec![42, 43]);
    state.reconcile_raw(
        &initial,
        &[first.clone(), vanished],
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let first_reference = state.model.live_reference(42).cloned().ok_or("missing")?;
    let vanished_reference = state.model.live_reference(43).cloned().ok_or("missing")?;
    let mut backend = ScriptedBackend::new(
        [Ok(initial)],
        [
            Ok(first),
            Err(RawBackendFailure::RequestFailed),
            Err(RawBackendFailure::RequestFailed),
        ],
    );

    reconcile_from_backend(
        &mut state,
        &mut backend,
        &ModelActorControl::default(),
        MonotonicMillis::new(2),
        Duration::from_millis(10),
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::Incremental,
    )?;

    assert_eq!(state.model.live_reference(42), Some(&first_reference));
    assert!(state.model.live_reference(43).is_none());
    assert!(matches!(
        state
            .model
            .resolve_exact(&vanished_reference, MonotonicMillis::new(3)),
        Err(WindowModelError::DestroyedReference)
    ));
    Ok(())
}

#[test]
fn multi_window_reconcile_uses_one_total_raw_event_budget() -> Result<(), Box<dyn Error>> {
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
    let first = raw(42)?;
    let second = raw(43)?;
    let mut within_budget = SequentialDelayBackend {
        inventory: inventory(InventorySource::NetClientList, vec![42, 43]),
        snapshots: VecDeque::from([first.clone(), second.clone()]),
        delay: Duration::from_millis(5),
    };
    process_raw_event(
        &mut state,
        &mut within_budget,
        &ModelActorControl::default(),
        ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::RebuildInventory,
        },
        &|| MonotonicMillis::new(1),
        Duration::from_millis(100),
    )?;
    let (_, snapshots) = state.model.snapshot_all(MonotonicMillis::new(1))?;
    assert_eq!(snapshots.len(), 2);

    let revision = state.model.revision();
    let old = state.model.live_reference(42).cloned().ok_or("missing")?;
    let mut exhausted = SequentialDelayBackend {
        inventory: inventory(InventorySource::NetClientList, vec![42, 43]),
        snapshots: VecDeque::from([first, second]),
        delay: Duration::from_millis(20),
    };
    let started = Instant::now();
    process_raw_event(
        &mut state,
        &mut exhausted,
        &ModelActorControl::default(),
        ObservationActorEvent::Reconcile {
            decision: ReconcileDecision::RebuildInventory,
        },
        &|| MonotonicMillis::new(2),
        Duration::from_millis(50),
    )?;
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_millis(150));
    assert!(state.model.revision() > revision);
    assert!(state.raw_resync_pending);
    assert!(matches!(
        state.model.resolve_exact(&old, MonotonicMillis::new(2)),
        Err(WindowModelError::DestroyedReference)
    ));
    Ok(())
}

#[test]
fn resync_waiters_receive_post_rebuild_revision_refs_and_tokens() -> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    let inventory = inventory(InventorySource::NetClientListStacking, vec![42]);
    state.reconcile_raw(
        &inventory,
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let old = state.model.live_reference(42).cloned().ok_or("missing")?;
    let request = WindowWaitRequest {
        desktop_id,
        desktop_generation: generation,
        target: WindowWaitTarget::Selector {
            selector: WindowSelector::Predicate {
                predicate: WindowPredicate::MapState {
                    value: WindowMapState::Viewable,
                },
            },
            quantifier: WindowWaitSelectorQuantifier::Any,
        },
        predicate: WindowWaitPredicate::Exists,
        after_revision: Some(state.model.revision()),
        timeout_ms: 100,
    };
    let (response, mut receiver) = oneshot::channel();
    let mut waiters = vec![PendingWait {
        principal: "alice".to_owned(),
        request,
        deadline: MonotonicMillis::new(100),
        wall_deadline: None,
        response,
    }];
    let mut backend = ScriptedBackend::new([Ok(inventory)], [Ok(input)]);

    process_actor_event(
        &mut state,
        &mut waiters,
        &mut backend,
        &ModelActorControl::default(),
        ObservationActorEvent::ResyncRequired,
        &|| MonotonicMillis::new(2),
        Duration::from_millis(10),
    )?;

    let result = receiver
        .try_recv()?
        .map_err(|_| "wait unexpectedly failed")?;
    assert_eq!(result.status, WindowWaitStatus::ResyncRequired);
    assert_eq!(result.evaluated_revision, state.model.revision());
    assert_eq!(result.windows.len(), 1);
    let entry = &result.windows[0];
    let replacement = entry.snapshot.window.clone();
    assert_ne!(replacement, old);
    assert_eq!(entry.snapshot.model_revision, result.evaluated_revision);
    assert_eq!(
        state.tokens.resolve_reference(
            &entry.reference_token,
            "alice",
            desktop_id,
            generation,
            MonotonicMillis::new(2),
        )?,
        replacement
    );
    assert!(matches!(
        state.model.resolve_exact(&old, MonotonicMillis::new(2)),
        Err(WindowModelError::StaleReference | WindowModelError::DestroyedReference)
    ));
    Ok(())
}

#[test]
fn cursor_claim_rejects_tamper_principal_scope_query_order_and_expiry() -> Result<(), Box<dyn Error>>
{
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let descriptor = WindowContinuationDescriptor {
        desktop_id,
        desktop_generation: generation,
        snapshot_revision: WindowModelRevision::new(7)?,
        order: WindowOrder::XidAscending,
        query: WindowContinuationQuery::List,
        next_offset: 1,
    };
    let correlation_epoch = 3;
    let mut registry =
        OpaqueTokenRegistry::with_entropy(4, 10, 10, Arc::new(ScriptedEntropy::new([[0x44; 32]])))?;
    let token = registry.mint_cursor(
        "alice",
        descriptor.clone(),
        Some(correlation_epoch),
        MonotonicMillis::new(5),
    )?;
    assert_eq!(
        registry.resolve_cursor(
            &token,
            "alice",
            desktop_id,
            generation,
            WindowOrder::XidAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        )?,
        descriptor
    );
    for rejected in [
        registry.resolve_cursor(
            &token,
            "mallory",
            desktop_id,
            generation,
            WindowOrder::XidAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        ),
        registry.resolve_cursor(
            &token,
            "alice",
            DesktopId::new(),
            generation,
            WindowOrder::XidAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        ),
        registry.resolve_cursor(
            &token,
            "alice",
            desktop_id,
            DesktopGeneration::new(),
            WindowOrder::XidAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        ),
        registry.resolve_cursor(
            &token,
            "alice",
            desktop_id,
            generation,
            WindowOrder::TitleAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        ),
        registry.resolve_cursor(
            &token,
            "alice",
            desktop_id,
            generation,
            WindowOrder::XidAscending,
            CursorQueryBinding::Selector([9; 32]),
            Some(correlation_epoch),
            MonotonicMillis::new(5),
        ),
        registry.resolve_cursor(
            &token,
            "alice",
            desktop_id,
            generation,
            WindowOrder::XidAscending,
            CursorQueryBinding::List,
            Some(correlation_epoch + 1),
            MonotonicMillis::new(5),
        ),
    ] {
        assert!(matches!(
            rejected,
            Err(ObservationAdapterError::TokenUnavailable)
        ));
    }
    let mut tampered = token.as_str().to_owned();
    tampered.replace_range(tampered.len() - 1.., "0");
    let tampered = WindowPageCursor::new(tampered)?;
    assert!(
        registry
            .resolve_cursor(
                &tampered,
                "alice",
                desktop_id,
                generation,
                WindowOrder::XidAscending,
                CursorQueryBinding::List,
                Some(correlation_epoch),
                MonotonicMillis::new(5),
            )
            .is_err()
    );
    assert!(
        registry
            .resolve_cursor(
                &token,
                "alice",
                desktop_id,
                generation,
                WindowOrder::XidAscending,
                CursorQueryBinding::List,
                Some(correlation_epoch),
                MonotonicMillis::new(15),
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn list_cursor_is_rejected_after_model_revision_drift() -> Result<(), Box<dyn Error>> {
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
    let inputs = [raw(42)?, raw(43)?];
    state.reconcile_raw(
        &inventory(InventorySource::NetClientListStacking, vec![42, 43]),
        &inputs,
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let mut request = WindowListRequest {
        desktop_id,
        desktop_generation: generation,
        limit: 1,
        order: WindowOrder::XidAscending,
        cursor: None,
    };
    let first = state
        .list("alice", &request, MonotonicMillis::new(1))
        .map_err(|_| "first list failed")?;
    request.cursor = first.next_cursor;
    let mut changed = inputs[0].clone();
    changed.properties.title = Some(WindowText::new("Changed", false)?);
    state.observe_raw(&changed, MonotonicMillis::new(2))?;
    assert!(matches!(
        state.list("alice", &request, MonotonicMillis::new(2)),
        Err(ControlPlaneError::NotFound)
    ));
    Ok(())
}

#[test]
fn list_query_resolve_snapshot_share_atomic_entries_and_exact_tokens() -> Result<(), Box<dyn Error>>
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
    let mut alpha = raw(42)?;
    alpha.properties.title = Some(WindowText::new("Alpha", false)?);
    let mut beta = raw(43)?;
    beta.properties.title = Some(WindowText::new("Beta", false)?);
    state.reconcile_raw(
        &inventory(InventorySource::NetClientListStacking, vec![42, 43]),
        &[alpha, beta],
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let selector = WindowSelector::Predicate {
        predicate: WindowPredicate::Text {
            field: WindowTextField::Title,
            matcher: WindowStringMatch::Exact {
                value: "Alpha".to_owned(),
                case_sensitive: true,
            },
        },
    };
    let list = state
        .list(
            "alice",
            &WindowListRequest {
                desktop_id,
                desktop_generation: generation,
                limit: MAX_WINDOW_PAGE_LIMIT,
                order: WindowOrder::XidAscending,
                cursor: None,
            },
            MonotonicMillis::new(1),
        )
        .map_err(|_| "list failed")?;
    assert_eq!(list.windows.len(), 2);
    assert!(
        list.windows
            .iter()
            .all(|entry| entry.snapshot.model_revision == list.snapshot_revision)
    );
    let query = state
        .query(
            "alice",
            &WindowQueryRequest {
                desktop_id,
                desktop_generation: generation,
                selector: selector.clone(),
                order: WindowOrder::XidAscending,
                limit: MAX_WINDOW_PAGE_LIMIT,
                cursor: None,
            },
            MonotonicMillis::new(1),
        )
        .map_err(|_| "query failed")?;
    assert_eq!(query.windows.len(), 1);
    let resolved = state
        .resolve(
            "alice",
            &WindowResolveRequest {
                desktop_id,
                desktop_generation: generation,
                selector,
                order: WindowOrder::XidAscending,
                match_policy: WindowSingleMatchPolicy::ExactlyOne,
            },
            MonotonicMillis::new(1),
        )
        .map_err(|_| "resolve failed")?;
    assert_eq!(resolved.window.snapshot.window.xid, 42);
    let lookup = state
        .snapshot(
            "alice",
            &WindowSnapshotRequest {
                desktop_id,
                desktop_generation: generation,
                target: WindowSnapshotTarget::Token {
                    token: resolved.window.reference_token.clone(),
                },
            },
            MonotonicMillis::new(1),
        )
        .map_err(|_| "snapshot failed")?;
    assert_eq!(
        lookup.window.snapshot.window,
        resolved.window.snapshot.window
    );
    assert!(matches!(
        state.snapshot(
            "mallory",
            &WindowSnapshotRequest {
                desktop_id,
                desktop_generation: generation,
                target: WindowSnapshotTarget::Token {
                    token: resolved.window.reference_token.clone(),
                },
            },
            MonotonicMillis::new(1),
        ),
        Err(ControlPlaneError::NotFound)
    ));
    let exact = resolved.window.snapshot.window;
    state.remove_xid(42, MonotonicMillis::new(2))?;
    assert!(matches!(
        state.snapshot(
            "alice",
            &WindowSnapshotRequest {
                desktop_id,
                desktop_generation: generation,
                target: WindowSnapshotTarget::Token {
                    token: resolved.window.reference_token,
                },
            },
            MonotonicMillis::new(2),
        ),
        Err(ControlPlaneError::NotFound)
    ));
    assert!(matches!(
        state.model.resolve_exact(&exact, MonotonicMillis::new(2)),
        Err(WindowModelError::DestroyedReference)
    ));
    Ok(())
}

fn wait_request(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    window: WindowRef,
    predicate: WindowWaitPredicate,
    after_revision: Option<WindowModelRevision>,
    timeout_ms: u32,
) -> WindowWaitRequest {
    WindowWaitRequest {
        desktop_id,
        desktop_generation: generation,
        target: WindowWaitTarget::Reference { window },
        predicate,
        after_revision,
        timeout_ms,
    }
}

#[test]
fn wait_matching_at_deadline_wins_over_timeout() -> Result<(), Box<dyn Error>> {
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
    let mut input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let window = state.model.live_reference(42).cloned().ok_or("missing")?;
    let (response, mut receiver) = oneshot::channel();
    let mut waiters = Vec::new();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: wait_request(
                desktop_id,
                generation,
                window,
                WindowWaitPredicate::Active { desired: true },
                None,
                5,
            ),
            deadline: None,
            response,
        },
        MonotonicMillis::new(1),
        2,
    );
    assert_eq!(waiters.len(), 1);
    input.root.active_window = Some(42);
    state.observe_raw(&input, MonotonicMillis::new(2))?;
    process_waiters(&mut state, &mut waiters, MonotonicMillis::new(6));

    let result = receiver.try_recv()?.map_err(|_| "wait failed")?;
    assert_eq!(result.status, WindowWaitStatus::Matched);
    assert!(result.predicate_satisfied);
    assert!(waiters.is_empty());
    Ok(())
}

#[test]
fn wait_nonmatching_at_deadline_times_out_instead_of_remaining_registered()
-> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let window = state.model.live_reference(42).cloned().ok_or("missing")?;
    let (response, mut receiver) = oneshot::channel();
    let mut waiters = Vec::new();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: wait_request(
                desktop_id,
                generation,
                window,
                WindowWaitPredicate::Focused { desired: true },
                None,
                5,
            ),
            deadline: None,
            response,
        },
        MonotonicMillis::new(1),
        2,
    );
    assert_eq!(waiters.len(), 1);
    process_waiters(&mut state, &mut waiters, MonotonicMillis::new(6));

    let result = receiver.try_recv()?.map_err(|_| "wait failed")?;
    assert_eq!(result.status, WindowWaitStatus::TimedOut);
    assert!(!result.predicate_satisfied);
    assert!(waiters.is_empty());
    Ok(())
}

#[test]
fn wait_check_register_recheck_honors_revision_boundary_and_immediate_match()
-> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let window = state.model.live_reference(42).cloned().ok_or("missing")?;
    let (immediate_tx, mut immediate_rx) = oneshot::channel();
    let mut waiters = Vec::new();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: wait_request(
                desktop_id,
                generation,
                window.clone(),
                WindowWaitPredicate::Active { desired: false },
                None,
                20,
            ),
            deadline: None,
            response: immediate_tx,
        },
        MonotonicMillis::new(1),
        2,
    );
    assert_eq!(
        immediate_rx
            .try_recv()?
            .map_err(|_| "immediate wait failed")?
            .status,
        WindowWaitStatus::Matched
    );
    assert!(waiters.is_empty());

    let boundary = state.model.revision();
    let (boundary_tx, mut boundary_rx) = oneshot::channel();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: wait_request(
                desktop_id,
                generation,
                window,
                WindowWaitPredicate::Active { desired: false },
                Some(boundary),
                20,
            ),
            deadline: None,
            response: boundary_tx,
        },
        MonotonicMillis::new(2),
        2,
    );
    assert_eq!(waiters.len(), 1);
    let mut changed = input;
    changed.properties.title = Some(WindowText::new("Changed", false)?);
    state.observe_raw(&changed, MonotonicMillis::new(3))?;
    process_waiters(&mut state, &mut waiters, MonotonicMillis::new(3));
    assert_eq!(
        boundary_rx
            .try_recv()?
            .map_err(|_| "boundary wait failed")?
            .status,
        WindowWaitStatus::Matched
    );
    assert!(waiters.is_empty());
    Ok(())
}

#[test]
fn wait_timeout_vanish_cancel_and_saturation_are_bounded() -> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let window = state.model.live_reference(42).cloned().ok_or("missing")?;
    let pending_request = wait_request(
        desktop_id,
        generation,
        window.clone(),
        WindowWaitPredicate::Focused { desired: true },
        None,
        5,
    );
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, mut second_rx) = oneshot::channel();
    let mut waiters = Vec::new();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: pending_request.clone(),
            deadline: None,
            response: first_tx,
        },
        MonotonicMillis::new(1),
        1,
    );
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: pending_request.clone(),
            deadline: None,
            response: second_tx,
        },
        MonotonicMillis::new(1),
        1,
    );
    assert!(matches!(
        second_rx.try_recv()?,
        Err(ControlPlaneError::ResourceExhausted)
    ));
    drop(first_rx);
    process_waiters(&mut state, &mut waiters, MonotonicMillis::new(2));
    assert!(waiters.is_empty());

    let (timeout_tx, mut timeout_rx) = oneshot::channel();
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Wait {
            principal: "alice".to_owned(),
            request: pending_request,
            deadline: None,
            response: timeout_tx,
        },
        MonotonicMillis::new(2),
        1,
    );
    process_waiters(&mut state, &mut waiters, MonotonicMillis::new(7));
    assert_eq!(
        timeout_rx
            .try_recv()?
            .map_err(|_| "timeout wait failed")?
            .status,
        WindowWaitStatus::TimedOut
    );

    state.remove_xid(42, MonotonicMillis::new(8))?;
    let vanished = state
        .evaluate_wait(
            "alice",
            &wait_request(
                desktop_id,
                generation,
                window,
                WindowWaitPredicate::Focused { desired: true },
                None,
                5,
            ),
            MonotonicMillis::new(8),
            None,
        )
        .map_err(|_| "vanish wait failed")?
        .ok_or("vanish was not terminal")?;
    assert_eq!(vanished.status, WindowWaitStatus::TargetVanished);
    Ok(())
}

#[test]
fn actor_is_object_safe_and_shutdown_is_prompt() -> Result<(), Box<dyn Error>> {
    fn require_object_safe(_plane: &dyn ObservationPlane) {}

    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let backend = ScriptedBackend::new(
        [Ok(inventory(InventorySource::NetClientList, Vec::new()))],
        [],
    );
    let (service, shutdown, join) = spawn_model_actor(
        Box::new(backend),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;
    require_object_safe(service.as_ref());
    assert_eq!(service.health(), ObservationServiceState::Healthy);
    let started = Instant::now();
    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    assert_eq!(service.health(), ObservationServiceState::Stopped);
    assert!(started.elapsed() < Duration::from_millis(500));
    Ok(())
}

#[tokio::test]
async fn service_exposes_atomic_correlation_and_blocking_queue_head_stacking_views()
-> Result<(), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let mut lower = raw(42)?;
    lower.geometry.client_rect =
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 10, 100, 100)?)?;
    let mut upper = raw(43)?;
    upper.geometry.client_rect =
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(50, 50, 100, 100)?)?;
    let backend = ScriptedBackend::new(
        [Ok(inventory(
            InventorySource::NetClientListStacking,
            vec![42, 43],
        ))],
        [Ok(lower.clone()), Ok(upper.clone())],
    );
    let (service, shutdown, join) = spawn_model_actor(
        Box::new(backend),
        desktop_id,
        generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;

    let correlation = service.accessibility_correlation_snapshot().await?;
    assert_eq!(correlation.windows.len(), 2);
    assert!(
        correlation
            .windows
            .iter()
            .all(|window| window.model_revision == correlation.revision)
    );
    let blocking_correlation =
        service.accessibility_correlation_snapshot_blocking(Duration::from_millis(250))?;
    assert_eq!(blocking_correlation.revision, correlation.revision);
    assert_eq!(
        blocking_correlation.windows.len(),
        correlation.windows.len()
    );
    let target = correlation
        .windows
        .iter()
        .find(|snapshot| snapshot.window.xid == 42)
        .map(|snapshot| snapshot.window.clone())
        .ok_or("missing target")?;
    let first =
        service.occlusion_snapshot_exact_blocking(target.clone(), Duration::from_millis(250))?;
    let second = service.occlusion_snapshot_exact_blocking(target, Duration::from_millis(250))?;
    assert_eq!(
        first.rectangles_above,
        vec![upper.geometry.client_rect.rect]
    );
    assert!(first.stacking_complete);
    assert!(second.stacking_epoch > first.stacking_epoch);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    Ok(())
}

#[test]
fn shutdown_wins_over_an_already_selected_failing_observation_event() -> Result<(), Box<dyn Error>>
{
    let gate = Arc::new((Mutex::new(ShutdownRaceState::default()), Condvar::new()));
    let backend = ShutdownRaceBackend {
        gate: Arc::clone(&gate),
    };
    let (service, shutdown, join) = spawn_model_actor(
        Box::new(backend),
        DesktopId::new(),
        DesktopGeneration::new(),
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;

    let (state, wake) = &*gate;
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !state.event_selected {
        state = wake
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    shutdown.request();
    state.release_event = true;
    wake.notify_all();
    drop(state);

    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    assert_eq!(service.health(), ObservationServiceState::Stopped);
    Ok(())
}

#[test]
fn effect_revalidation_requests_return_only_the_exact_live_birth() -> Result<(), Box<dyn Error>> {
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
    let input = raw(42)?;
    state.reconcile_raw(
        &inventory(InventorySource::NetClientList, vec![42]),
        std::slice::from_ref(&input),
        MonotonicMillis::new(1),
        ReconcileIdentityPolicy::PreserveContinuity,
    )?;
    let exact = state.model.live_reference(42).cloned().ok_or("missing")?;
    let mut waiters = Vec::new();
    let (revalidate_tx, revalidate_rx) = std::sync::mpsc::sync_channel(1);
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Revalidate {
            window: exact.clone(),
            response: revalidate_tx,
        },
        MonotonicMillis::new(2),
        2,
    );
    assert_eq!(
        revalidate_rx
            .recv_timeout(Duration::from_millis(100))?
            .map_err(|_| std::io::Error::other("revalidation failed"))?,
        42
    );
    let (snapshot_tx, snapshot_rx) = std::sync::mpsc::sync_channel(1);
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::InternalSnapshot {
            window: exact.clone(),
            response: snapshot_tx,
        },
        MonotonicMillis::new(2),
        2,
    );
    assert_eq!(
        snapshot_rx
            .recv_timeout(Duration::from_millis(100))?
            .map_err(|_| std::io::Error::other("snapshot failed"))?
            .window,
        exact
    );

    let mut stale = exact;
    stale.observed_generation = stale.observed_generation.saturating_add(1);
    let (stale_tx, stale_rx) = std::sync::mpsc::sync_channel(1);
    process_model_request(
        &mut state,
        &mut waiters,
        ModelRequest::Revalidate {
            window: stale,
            response: stale_tx,
        },
        MonotonicMillis::new(2),
        2,
    );
    assert_eq!(
        stale_rx.recv_timeout(Duration::from_millis(100))?,
        Err(ControlPlaneError::NotFound)
    );
    Ok(())
}

#[test]
fn dropping_join_during_later_startup_failure_stops_actor_with_service_alive()
-> Result<(), Box<dyn Error>> {
    let backend = ScriptedBackend::new(
        [Ok(inventory(InventorySource::NetClientList, Vec::new()))],
        [],
    );
    let (_service, _shutdown, join) = spawn_model_actor(
        Box::new(backend),
        DesktopId::new(),
        DesktopGeneration::new(),
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;

    let started = Instant::now();
    drop(join);
    assert!(started.elapsed() < Duration::from_millis(500));
    Ok(())
}

#[test]
fn actor_reports_backend_poison_and_panic_terminal_states() -> Result<(), Box<dyn Error>> {
    let empty_inventory = || inventory(InventorySource::NetClientList, Vec::new());
    let (poisoned_service, _shutdown, poisoned_join) = spawn_model_actor(
        Box::new(
            ScriptedBackend::new([Ok(empty_inventory())], [])
                .with_events([Err(RawBackendFailure::Unavailable)]),
        ),
        DesktopId::new(),
        DesktopGeneration::new(),
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;
    assert_eq!(poisoned_join.join(), ObservationServiceExit::Poisoned);
    assert_eq!(poisoned_service.health(), ObservationServiceState::Poisoned);

    let (panic_service, _shutdown, panic_join) = spawn_model_actor(
        Box::new(ScriptedBackend::new([Ok(empty_inventory())], []).with_event_panic()),
        DesktopId::new(),
        DesktopGeneration::new(),
        WindowModelLimits::default(),
        ObservationServiceSettings::for_test(),
    )?;
    assert_eq!(panic_join.join(), ObservationServiceExit::Panicked);
    assert_eq!(panic_service.health(), ObservationServiceState::Panicked);
    Ok(())
}

#[test]
fn startup_and_settings_fail_closed() {
    let mut settings = ObservationServiceSettings::for_test();
    settings.request_capacity = 0;
    assert!(matches!(
        spawn_model_actor(
            Box::new(ScriptedBackend::new([], [])),
            DesktopId::new(),
            DesktopGeneration::new(),
            WindowModelLimits::default(),
            settings,
        ),
        Err(ObservationCompositionError::InvalidSettings)
    ));

    let mut overflowing_settings = ObservationServiceSettings::for_test();
    overflowing_settings.raw_request_timeout = Duration::MAX;
    assert!(matches!(
        spawn_model_actor(
            Box::new(ScriptedBackend::new([], [])),
            DesktopId::new(),
            DesktopGeneration::new(),
            WindowModelLimits::default(),
            overflowing_settings,
        ),
        Err(ObservationCompositionError::InvalidSettings)
    ));

    let invalid_inventory = RootInventory {
        windows: Vec::new(),
        source: InventorySource::NetClientList,
        warnings: vec![InventoryWarning::Truncated],
    };
    assert!(matches!(
        spawn_model_actor(
            Box::new(ScriptedBackend::new([Ok(invalid_inventory)], [])),
            DesktopId::new(),
            DesktopGeneration::new(),
            WindowModelLimits::default(),
            ObservationServiceSettings::for_test(),
        ),
        Err(ObservationCompositionError::InitialReconcileFailed)
    ));
}
