use std::{
    error::Error,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use xenoteer_protocol::{
    CoordinateSpace, MAX_WINDOW_CHANGED_FIELDS, MAX_WINDOW_TEXT_BYTES, Rect, SCREEN_DAMAGED_TOPIC,
    ScreenDamageCoverage, ScreenDamageEvent, WindowClass, WindowFocusEvent, WindowGeometryEvent,
    WindowLifecycleEvent, WindowLifecycleKind, WindowMapState, WindowMetadataEvent,
    WindowMetadataField, WindowModelRebuildReason, WindowModelRebuiltEvent, WindowRect, WindowText,
};
use xenoteer_x11::{
    FocusAncestryStatus, ObservedAtom, ObservedPropertyWarning, PropertyWarning, RootDamageBatch,
    RootDamageCoverage, RootDamageRect, RootGeometryInput, RootWindowEvidenceInput,
    WindowAttributeInput, WindowPropertyInput, WindowSnapshotInput,
};

use super::*;

#[derive(Clone, Copy)]
enum SinkMode {
    Accept,
    Full,
    Closed,
}

struct FakeWindowEventSink {
    mode: Mutex<SinkMode>,
    events: Mutex<Vec<NormalizedEvent>>,
    resync_requests: AtomicU64,
}

impl FakeWindowEventSink {
    fn new(mode: SinkMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            events: Mutex::new(Vec::new()),
            resync_requests: AtomicU64::new(0),
        }
    }

    fn events(&self) -> Vec<NormalizedEvent> {
        lock_unpoisoned(&self.events).clone()
    }

    fn clear(&self) {
        lock_unpoisoned(&self.events).clear();
    }

    fn resync_requests(&self) -> u64 {
        self.resync_requests.load(Ordering::Relaxed)
    }
}

impl WindowEventSink for FakeWindowEventSink {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        match *lock_unpoisoned(&self.mode) {
            SinkMode::Accept => {
                lock_unpoisoned(&self.events).push(event);
                Ok(())
            }
            SinkMode::Full => Err(WindowEventSinkError::Full),
            SinkMode::Closed => Err(WindowEventSinkError::Closed),
        }
    }

    fn require_resync(&self) {
        self.resync_requests.fetch_add(1, Ordering::Relaxed);
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

fn inventory(inputs: &[WindowSnapshotInput]) -> RootInventory {
    RootInventory {
        windows: inputs.iter().map(|input| input.window).collect(),
        source: InventorySource::NetClientListStacking,
        warnings: Vec::new(),
    }
}

fn state(
    sink: Arc<dyn WindowEventSink>,
) -> Result<(ModelState, DesktopId, DesktopGeneration), Box<dyn Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let state = ModelState::new_with_event_sink(
        desktop_id,
        generation,
        WindowModelLimits::default(),
        usize::from(MAX_WINDOW_PAGE_LIMIT),
        60_000,
        60_000,
        sink,
    )?;
    Ok((state, desktop_id, generation))
}

fn reconcile(
    state: &mut ModelState,
    inputs: &[WindowSnapshotInput],
    now: u64,
    identity: ReconcileIdentityPolicy,
    events: ReconcileEventPolicy,
) -> Result<(), ObservationAdapterError> {
    state.reconcile_raw_with_event_policy(
        &inventory(inputs),
        inputs,
        MonotonicMillis::new(now),
        identity,
        events,
    )
}

fn assert_sequence_free(events: &[NormalizedEvent]) {
    for event in events {
        assert!(event.payload.get("sequence").is_none());
        assert!(event.validate().is_ok(), "normalized event remains valid");
    }
}

fn damage_rect(x: i32, y: i32, width: u32, height: u32) -> Result<RootDamageRect, Box<dyn Error>> {
    RootDamageRect::new(x, y, width, height)
        .ok_or_else(|| std::io::Error::other("invalid test damage rectangle").into())
}

#[test]
fn initial_reconcile_emits_one_startup_baseline_not_created_bursts() -> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, desktop_id, generation) = state(sink.clone())?;
    reconcile(
        &mut state,
        &[raw(42)?, raw(84)?],
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic.as_str(), WINDOW_MODEL_REBUILT_TOPIC);
    let rebuilt: WindowModelRebuiltEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(rebuilt.desktop_id, desktop_id);
    assert_eq!(rebuilt.desktop_generation, generation);
    assert_eq!(rebuilt.previous_revision, None);
    assert_eq!(rebuilt.window_count, 2);
    assert_eq!(rebuilt.reason, WindowModelRebuildReason::Startup);
    rebuilt.validate()?;
    assert_sequence_free(&events);
    Ok(())
}

#[test]
fn event_loss_remints_xids_in_destroy_create_order_and_rejects_stale_refs()
-> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    let first = raw(42)?;
    let mut dependent = raw(84)?;
    dependent.properties.client_leader = Some(42);
    reconcile(
        &mut state,
        &[first.clone(), dependent.clone()],
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;
    sink.clear();
    let old_42 = state.model.live_reference(42).cloned().ok_or("old 42")?;
    let old_84 = state.model.live_reference(84).cloned().ok_or("old 84")?;

    reconcile(
        &mut state,
        &[first, dependent],
        2,
        ReconcileIdentityPolicy::InvalidateAll,
        ReconcileEventPolicy::Rebuilt(WindowModelRebuildReason::EventOverflow),
    )?;

    let events = sink.events();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].topic.as_str(), WINDOW_DESTROYED_TOPIC);
    assert_eq!(events[1].topic.as_str(), WINDOW_DESTROYED_TOPIC);
    assert_eq!(events[2].topic.as_str(), WINDOW_CREATED_TOPIC);
    assert_eq!(events[3].topic.as_str(), WINDOW_CREATED_TOPIC);
    assert_eq!(events[4].topic.as_str(), WINDOW_MODEL_REBUILT_TOPIC);
    let destroyed_42: WindowLifecycleEvent = serde_json::from_value(events[0].payload.clone())?;
    let destroyed_84: WindowLifecycleEvent = serde_json::from_value(events[1].payload.clone())?;
    let created_42: WindowLifecycleEvent = serde_json::from_value(events[2].payload.clone())?;
    let created_84: WindowLifecycleEvent = serde_json::from_value(events[3].payload.clone())?;
    assert_eq!(destroyed_42.window, old_42);
    assert_eq!(destroyed_84.window, old_84);
    assert_eq!(destroyed_42.lifecycle, WindowLifecycleKind::Destroyed);
    assert_eq!(created_42.lifecycle, WindowLifecycleKind::Created);
    assert_eq!(created_42.window.xid, 42);
    assert_eq!(created_84.window.xid, 84);
    assert_ne!(created_42.window, destroyed_42.window);
    assert_ne!(created_84.window, destroyed_84.window);
    let rebuilt: WindowModelRebuiltEvent = serde_json::from_value(events[4].payload.clone())?;
    assert_eq!(rebuilt.reason, WindowModelRebuildReason::EventOverflow);
    assert_eq!(rebuilt.window_count, 2);
    assert!(rebuilt.previous_revision.is_some());

    assert!(matches!(
        state.model.resolve_exact(&old_42, MonotonicMillis::new(3)),
        Err(WindowModelError::StaleReference)
    ));
    let current_84 = state.model.live_reference(84).cloned().ok_or("new 84")?;
    let resolved_84 = state
        .model
        .resolve_exact(&current_84, MonotonicMillis::new(3))?;
    assert_eq!(
        resolved_84.snapshot.client_leader,
        Some(created_42.window.clone())
    );
    assert_sequence_free(&events);
    Ok(())
}

#[test]
fn focus_descendants_are_normalized_once_and_equivalent_refreshes_are_suppressed()
-> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, desktop_id, generation) = state(sink.clone())?;
    reconcile(
        &mut state,
        &[raw(42)?, raw(84)?],
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;
    sink.clear();

    let mut focused = raw(42)?;
    focused.root.active_window = Some(42);
    focused.root.raw_focused_window = Some(4200);
    focused.root.focused_window = Some(42);
    focused.root.target_contains_focus = true;
    focused.root.focus_ancestry_status = FocusAncestryStatus::Resolved;
    let mut other = raw(84)?;
    other.root.active_window = Some(42);
    other.root.raw_focused_window = Some(4200);
    other.root.focused_window = Some(42);
    other.root.focus_ancestry_status = FocusAncestryStatus::Resolved;
    reconcile(
        &mut state,
        &[focused.clone(), other.clone()],
        2,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::Incremental,
    )?;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic.as_str(), WINDOW_CHANGED_TOPIC);
    let event: WindowFocusEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(event.desktop_id, desktop_id);
    assert_eq!(event.desktop_generation, generation);
    assert_eq!(event.previous_active, None);
    assert_eq!(event.previous_focused, None);
    assert_eq!(event.active.as_ref().map(|window| window.xid), Some(42));
    assert_eq!(event.focused.as_ref().map(|window| window.xid), Some(42));
    event.validate()?;
    sink.clear();

    focused.root.raw_focused_window = Some(4201);
    other.root.raw_focused_window = Some(4201);
    reconcile(
        &mut state,
        &[focused, other],
        3,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::Incremental,
    )?;
    assert!(sink.events().is_empty());
    Ok(())
}

#[test]
fn metadata_changes_are_unique_bounded_and_preserve_truncated_text_evidence()
-> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    reconcile(
        &mut state,
        &[raw(42)?, raw(84)?],
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;
    sink.clear();

    let bounded_title = "t".repeat(MAX_WINDOW_TEXT_BYTES);
    let mut changed = raw(42)?;
    changed.properties.title = Some(WindowText::new(bounded_title.clone(), true)?);
    changed.properties.visible_title = Some(WindowText::new("visible", false)?);
    changed.properties.icon_title = Some(WindowText::new("icon", false)?);
    changed.properties.class = Some(WindowClass {
        instance: Some(WindowText::new("instance", false)?),
        class: Some(WindowText::new("Class", false)?),
    });
    changed.properties.client_machine = Some(WindowText::new("machine", false)?);
    changed.properties.window_types = vec![ObservedAtom {
        id: 101,
        known: Some(KnownAtom::NetWmWindowTypeDialog),
    }];
    changed.properties.states = vec![ObservedAtom {
        id: 102,
        known: Some(KnownAtom::NetWmStateHidden),
    }];
    changed.properties.allowed_actions = vec![ObservedAtom {
        id: 103,
        known: Some(KnownAtom::NetWmActionClose),
    }];
    changed.properties.protocols = vec![ObservedAtom {
        id: 104,
        known: Some(KnownAtom::WmDeleteWindow),
    }];
    changed.properties.workspace = Some(7);
    changed.properties.client_leader = Some(84);
    changed.properties.reported_pid = Some(2222);
    changed.properties.urgent = true;
    changed.properties.warnings = vec![ObservedPropertyWarning {
        property: KnownAtom::NetWmName,
        warning: PropertyWarning::Truncated,
    }];
    state.observe_raw(&changed, MonotonicMillis::new(2))?;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    let event: WindowMetadataEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(
        event.changed,
        vec![
            WindowMetadataField::Title,
            WindowMetadataField::VisibleTitle,
            WindowMetadataField::IconTitle,
            WindowMetadataField::Class,
            WindowMetadataField::ClientMachine,
            WindowMetadataField::WindowTypes,
            WindowMetadataField::States,
            WindowMetadataField::AllowedActions,
            WindowMetadataField::Protocols,
            WindowMetadataField::Workspace,
            WindowMetadataField::Relationships,
            WindowMetadataField::ProcessCorrelation,
        ]
    );
    assert!(event.changed.len() <= MAX_WINDOW_CHANGED_FIELDS);
    assert_eq!(
        event.metadata.title.as_ref().ok_or("metadata title")?.value,
        bounded_title
    );
    assert!(event.metadata.title.as_ref().ok_or("title")?.lossy);
    event.validate()?;
    assert_sequence_free(&events);
    Ok(())
}

#[test]
fn root_physical_geometry_changes_emit_checked_before_and_after() -> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    reconcile(
        &mut state,
        &[raw(42)?],
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;
    sink.clear();

    let mut moved = raw(42)?;
    moved.geometry.client_rect =
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(55, 66, 800, 600)?)?;
    state.observe_raw(&moved, MonotonicMillis::new(2))?;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    let event: WindowGeometryEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(
        event.before.as_ref().ok_or("before geometry")?.client_rect,
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 20, 640, 480)?,)?
    );
    assert_eq!(event.after.client_rect, moved.geometry.client_rect);
    event.validate()?;
    assert_sequence_free(&events);
    Ok(())
}

#[test]
fn duplicate_refreshes_are_semantic_no_ops() -> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    let unchanged = raw(42)?;
    reconcile(
        &mut state,
        std::slice::from_ref(&unchanged),
        1,
        ReconcileIdentityPolicy::PreserveContinuity,
        ReconcileEventPolicy::InitialBaseline,
    )?;
    sink.clear();
    let before_revision = state.model.revision();
    state.observe_raw(&unchanged, MonotonicMillis::new(2))?;
    assert_eq!(state.model.revision(), before_revision);
    assert!(sink.events().is_empty());
    Ok(())
}

#[test]
fn full_and_closed_sinks_drop_without_blocking_or_poisoning_model() -> Result<(), Box<dyn Error>> {
    for (mode, expected) in [
        (
            SinkMode::Full,
            WindowEventDropStats {
                full: 1,
                closed: 0,
                invalid: 0,
            },
        ),
        (
            SinkMode::Closed,
            WindowEventDropStats {
                full: 0,
                closed: 1,
                invalid: 0,
            },
        ),
    ] {
        let sink = Arc::new(FakeWindowEventSink::new(mode));
        let (mut state, _, _) = state(sink.clone())?;
        state.observe_raw(&raw(42)?, MonotonicMillis::new(1))?;
        assert!(state.model.live_reference(42).is_some());
        assert_eq!(state.event_drop_stats(), expected);
        assert_eq!(sink.resync_requests(), 1);
        state.observe_raw(&raw(42)?, MonotonicMillis::new(2))?;
        assert!(state.model.live_reference(42).is_some());
        assert_eq!(state.event_drop_stats(), expected);
    }
    Ok(())
}

#[test]
fn removing_an_unknown_or_already_removed_xid_emits_nothing() -> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    state.remove_xid(999, MonotonicMillis::new(1))?;
    assert!(sink.events().is_empty());
    state.observe_raw(&raw(42)?, MonotonicMillis::new(2))?;
    sink.clear();
    state.remove_xid(42, MonotonicMillis::new(3))?;
    let events = sink.events();
    assert_eq!(events.len(), 1);
    let destroyed: WindowLifecycleEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(destroyed.lifecycle, WindowLifecycleKind::Destroyed);
    sink.clear();
    state.remove_xid(42, MonotonicMillis::new(4))?;
    assert!(sink.events().is_empty());
    Ok(())
}

#[test]
fn root_damage_batches_map_every_coverage_to_checked_root_physical_events()
-> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (state, desktop_id, generation) = state(sink.clone())?;
    let root = damage_rect(0, 0, 800, 600)?;
    let cases = [
        (
            RootDamageCoverage::Regions,
            vec![damage_rect(10, 20, 30, 40)?, damage_rect(100, 120, 50, 60)?],
            ScreenDamageCoverage::Regions,
        ),
        (
            RootDamageCoverage::BoundingBox,
            vec![damage_rect(10, 20, 300, 200)?],
            ScreenDamageCoverage::BoundingBox,
        ),
        (
            RootDamageCoverage::FullScreen,
            vec![root],
            ScreenDamageCoverage::FullScreen,
        ),
    ];
    for (raw_coverage, regions, public_coverage) in cases {
        sink.clear();
        state.emit_root_damage(RootDamageBatch {
            root_region: root,
            regions,
            coverage: raw_coverage,
            notifications: 7,
        });
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].topic.as_str(), SCREEN_DAMAGED_TOPIC);
        let event: ScreenDamageEvent = serde_json::from_value(events[0].payload.clone())?;
        assert_eq!(event.desktop_id, desktop_id);
        assert_eq!(event.desktop_generation, generation);
        assert_eq!(event.coverage, public_coverage);
        assert_eq!(event.coalesced_notifications, 7);
        assert_eq!(
            event.root_region.coordinate_space,
            CoordinateSpace::RootPhysical
        );
        assert!(
            event
                .damaged_regions
                .iter()
                .all(|region| region.coordinate_space == CoordinateSpace::RootPhysical)
        );
        event.validate()?;
        assert_sequence_free(&events);
    }
    Ok(())
}

#[test]
fn invalid_root_damage_requests_resync_without_emitting_or_poisoning_state()
-> Result<(), Box<dyn Error>> {
    let sink = Arc::new(FakeWindowEventSink::new(SinkMode::Accept));
    let (mut state, _, _) = state(sink.clone())?;
    state.emit_root_damage(RootDamageBatch {
        root_region: damage_rect(0, 0, 800, 600)?,
        regions: vec![damage_rect(10, 10, 20, 20)?],
        coverage: RootDamageCoverage::Regions,
        notifications: 0,
    });
    assert!(sink.events().is_empty());
    assert_eq!(
        state.event_drop_stats(),
        WindowEventDropStats {
            full: 0,
            closed: 0,
            invalid: 1,
        }
    );
    assert_eq!(sink.resync_requests(), 1);
    state.observe_raw(&raw(42)?, MonotonicMillis::new(1))?;
    assert!(state.model.live_reference(42).is_some());
    Ok(())
}
