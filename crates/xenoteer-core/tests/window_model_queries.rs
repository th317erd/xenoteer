//! Cross-module proofs for atomic window-model query projections.

use xenoteer_core::{MonotonicMillis, WindowModel, WindowModelLimits};
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, WindowIdentityHash, WindowMapState, WindowMetadata,
    WindowModelRevision, WindowObservedState, WindowProcessConfidence, WindowProcessCorrelation,
    WindowRef, WindowSnapshot,
};

fn snapshot(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    xid: u32,
    observed_generation: u64,
    identity_byte: char,
) -> Result<WindowSnapshot, Box<dyn std::error::Error>> {
    let window = WindowRef {
        desktop_id,
        desktop_generation,
        xid,
        observed_generation,
        identity_hash: WindowIdentityHash::new(identity_byte.to_string().repeat(64))?,
    };
    Ok(WindowSnapshot {
        xid_hex: window.xid_hex(),
        window,
        model_revision: WindowModelRevision::new(1)?,
        metadata: WindowMetadata {
            title: None,
            visible_title: None,
            icon_title: None,
            class: None,
            client_machine: None,
            window_types: Vec::new(),
            states: Vec::new(),
            allowed_actions: Vec::new(),
            protocols: Vec::new(),
        },
        process: WindowProcessCorrelation {
            reported_pid: None,
            managed_process: None,
            confidence: WindowProcessConfidence::None,
            evidence: Vec::new(),
            conflict: false,
        },
        state: WindowObservedState {
            map_state: WindowMapState::Viewable,
            minimized: false,
            hidden: false,
            urgent: false,
            modal: false,
            sticky: false,
            active: false,
            focused: false,
        },
        geometry: None,
        workspace: None,
        client_leader: None,
        transient_for: None,
        group_leader: None,
        stacking_index: None,
        has_accessibility_application: false,
        warnings: Vec::new(),
    })
}

#[test]
fn atomic_views_stamp_every_snapshot_with_the_common_revision()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let mut model = WindowModel::new(desktop_id, desktop_generation, WindowModelLimits::default())?;
    let first = snapshot(desktop_id, desktop_generation, 10, 1, 'a')?;
    let first_reference = first.window.clone();
    model.observe(first, MonotonicMillis::new(1))?;
    model.observe(
        snapshot(desktop_id, desktop_generation, 11, 2, 'b')?,
        MonotonicMillis::new(2),
    )?;

    let (revision, snapshots) = model.snapshot_all(MonotonicMillis::new(3))?;
    assert_eq!(revision, WindowModelRevision::new(3)?);
    assert_eq!(snapshots.len(), 2);
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot.model_revision == revision)
    );
    let resolved = model.resolve_exact(&first_reference, MonotonicMillis::new(4))?;
    assert_eq!(resolved.revision, revision);
    assert_eq!(resolved.snapshot.model_revision, revision);
    Ok(())
}

#[test]
fn global_birth_serial_prevents_retargeting_after_history_eviction()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let mut model = WindowModel::new(
        desktop_id,
        desktop_generation,
        WindowModelLimits {
            max_live_windows: 2,
            max_tombstones: 1,
            tombstone_ttl_ms: 1,
        },
    )?;
    let first = snapshot(desktop_id, desktop_generation, 42, 1, 'a')?;
    let old_reference = first.window.clone();
    model.observe(first, MonotonicMillis::new(1))?;
    model.destroy(&old_reference, MonotonicMillis::new(2))?;

    let intervening = snapshot(desktop_id, desktop_generation, 99, 2, 'b')?;
    let intervening_reference = intervening.window.clone();
    model.observe(intervening, MonotonicMillis::new(3))?;
    model.destroy(&intervening_reference, MonotonicMillis::new(4))?;

    let reused = snapshot(desktop_id, desktop_generation, 42, 3, 'c')?;
    model.observe(reused, MonotonicMillis::new(5))?;
    assert_eq!(
        model.resolve_exact(&old_reference, MonotonicMillis::new(6)),
        Err(xenoteer_core::WindowModelError::StaleReference)
    );
    Ok(())
}

#[test]
fn creation_revision_survives_refresh_and_resets_for_xid_reuse()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let mut model = WindowModel::new(desktop_id, desktop_generation, WindowModelLimits::default())?;
    let first = snapshot(desktop_id, desktop_generation, 42, 1, 'c')?;
    let first_reference = first.window.clone();
    model.observe(first.clone(), MonotonicMillis::new(1))?;
    model.observe(first, MonotonicMillis::new(2))?;

    let (revision, records) = model.snapshot_query_records(MonotonicMillis::new(3))?;
    assert_eq!(revision, WindowModelRevision::new(3)?);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].created_revision, WindowModelRevision::new(2)?);
    assert_eq!(records[0].snapshot.model_revision, revision);

    model.destroy(&first_reference, MonotonicMillis::new(4))?;
    model.observe(
        snapshot(desktop_id, desktop_generation, 42, 2, 'd')?,
        MonotonicMillis::new(5),
    )?;
    let (reused_revision, reused_records) =
        model.snapshot_query_records(MonotonicMillis::new(6))?;
    assert_eq!(reused_revision, WindowModelRevision::new(5)?);
    assert_eq!(reused_records.len(), 1);
    assert_eq!(reused_records[0].created_revision, reused_revision);
    assert_eq!(reused_records[0].snapshot.window.observed_generation, 2);
    Ok(())
}
