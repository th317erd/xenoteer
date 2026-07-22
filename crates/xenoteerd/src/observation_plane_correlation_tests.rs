//! Adversarial tests for advisory processd/window PID correlation.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    sync::Mutex,
};

use xenoteer_processd::{
    BrokerPidCorrelation, BrokerPidCorrelationEvidence, MAX_PROCESS_CORRELATION_PIDS,
};
use xenoteer_protocol::{
    CoordinateSpace, LaunchId, ProcessRef, Rect, WindowIdentityHash, WindowListPage,
    WindowMapState, WindowModelRevision, WindowQueryPage, WindowRect, WindowReferenceToken,
    WindowResolveResult, WindowSnapshotResult,
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

impl PidCorrelator for PendingPidCorrelator {
    fn correlate<'a>(&'a self, _: DesktopGeneration, pids: Vec<u32>) -> PidCorrelationFuture<'a> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(pids);
        Box::pin(std::future::pending())
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
