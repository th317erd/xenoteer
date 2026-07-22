//! Deterministic adversarial tests for manager-owned PID correlation.

use std::collections::BTreeMap;

use super::*;
use crate::MAX_PROCESS_CORRELATION_PIDS;

fn process(generation: DesktopGeneration, pid: u32, start_ticks: u64) -> ProcessRef {
    ProcessRef::from_parts(generation, pid, start_ticks, Uuid::new_v4())
}

fn correlate(
    manager_generation: DesktopGeneration,
    requested_generation: DesktopGeneration,
    pids: &[u32],
    managed: &[ProcessRef],
    identities: &BTreeMap<u32, ProcIdentity>,
) -> Result<Vec<ManagedPidCorrelation>, ProcessManagerError> {
    correlate_pid_identities(
        manager_generation,
        requested_generation,
        pids,
        managed,
        |pid| {
            identities
                .get(&pid)
                .copied()
                .ok_or(ProcReadError::Unavailable)
        },
    )
}

#[test]
fn batch_validation_rejects_wrong_generation_zero_duplicate_empty_and_excess()
-> Result<(), ProcessManagerError> {
    let generation = DesktopGeneration::new();
    let other_generation = DesktopGeneration::new();
    let identities = BTreeMap::new();
    assert!(matches!(
        correlate(generation, other_generation, &[10], &[], &identities),
        Err(ProcessManagerError::WrongDesktopGeneration)
    ));
    assert!(matches!(
        correlate(
            generation,
            DesktopGeneration::from_uuid(Uuid::nil()),
            &[10],
            &[],
            &identities,
        ),
        Err(ProcessManagerError::InvalidCorrelationBatch)
    ));
    assert!(matches!(
        correlate(
            generation,
            generation,
            &[10],
            &[process(other_generation, 10, 1)],
            &identities,
        ),
        Err(ProcessManagerError::EventHistoryInvariant)
    ));
    for pids in [Vec::new(), vec![0], vec![10, 10]] {
        assert!(matches!(
            correlate(generation, generation, &pids, &[], &identities),
            Err(ProcessManagerError::InvalidCorrelationBatch)
        ));
    }
    let maximum = u32::try_from(MAX_PROCESS_CORRELATION_PIDS + 1)
        .map_err(|_| ProcessManagerError::InvalidCorrelationBatch)?;
    let excess = (1..=maximum).collect::<Vec<_>>();
    assert!(matches!(
        correlate(generation, generation, &excess, &[], &identities),
        Err(ProcessManagerError::InvalidCorrelationBatch)
    ));
    Ok(())
}

#[test]
fn exact_leader_descendant_and_unmanaged_live_pid_are_distinct() -> Result<(), ProcessManagerError>
{
    let generation = DesktopGeneration::new();
    let managed = process(generation, 100, 1_000);
    let identities = BTreeMap::from([
        (
            100,
            ProcIdentity {
                process_group: 100,
                start_ticks: 1_000,
            },
        ),
        (
            101,
            ProcIdentity {
                process_group: 100,
                start_ticks: 1_001,
            },
        ),
        (
            200,
            ProcIdentity {
                process_group: 200,
                start_ticks: 2_000,
            },
        ),
    ]);
    assert_eq!(
        correlate(
            generation,
            generation,
            &[100, 101, 200],
            std::slice::from_ref(&managed),
            &identities,
        )?,
        vec![
            ManagedPidCorrelation {
                pid: 100,
                evidence: ManagedPidCorrelationEvidence::Leader(managed.clone()),
            },
            ManagedPidCorrelation {
                pid: 101,
                evidence: ManagedPidCorrelationEvidence::ProcessGroup(managed),
            },
            ManagedPidCorrelation {
                pid: 200,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
        ]
    );
    Ok(())
}

#[test]
fn reused_leader_pid_never_falls_through_to_group_match() -> Result<(), ProcessManagerError> {
    let generation = DesktopGeneration::new();
    let managed = process(generation, 100, 1_000);
    let identities = BTreeMap::from([(
        100,
        ProcIdentity {
            process_group: 100,
            start_ticks: 9_999,
        },
    )]);
    assert_eq!(
        correlate(generation, generation, &[100], &[managed], &identities,)?,
        vec![ManagedPidCorrelation {
            pid: 100,
            evidence: ManagedPidCorrelationEvidence::NoMatch,
        }]
    );
    Ok(())
}

#[test]
fn ambiguous_group_still_fails_the_complete_batch() {
    let generation = DesktopGeneration::new();
    let first = process(generation, 100, 1_000);
    let second = process(generation, 100, 2_000);
    let identities = BTreeMap::from([
        (
            100,
            ProcIdentity {
                process_group: 100,
                start_ticks: 1_000,
            },
        ),
        (
            101,
            ProcIdentity {
                process_group: 100,
                start_ticks: 1_001,
            },
        ),
    ]);
    assert!(matches!(
        correlate(
            generation,
            generation,
            &[101],
            &[first.clone(), second],
            &identities,
        ),
        Err(ProcessManagerError::AmbiguousProcessGroup)
    ));
}

#[test]
fn valid_entries_survive_vanished_malformed_and_reused_proc_identities()
-> Result<(), ProcessManagerError> {
    let generation = DesktopGeneration::new();
    let valid = process(generation, 100, 1_000);
    let reused = process(generation, 300, 3_000);
    let vanished = process(generation, 400, 4_000);
    let malformed = process(generation, 500, 5_000);
    let correlations = correlate_pid_identities(
        generation,
        generation,
        &[100, 200, 201, 301, 401, 501, 101],
        &[valid.clone(), reused, vanished, malformed],
        |pid| match pid {
            100 => Ok(ProcIdentity {
                process_group: 100,
                start_ticks: 1_000,
            }),
            200 => Err(ProcReadError::Unavailable),
            201 => Err(ProcReadError::Malformed),
            301 => Ok(ProcIdentity {
                process_group: 300,
                start_ticks: 3_001,
            }),
            300 => Ok(ProcIdentity {
                process_group: 300,
                start_ticks: 9_999,
            }),
            401 => Ok(ProcIdentity {
                process_group: 400,
                start_ticks: 4_001,
            }),
            400 => Err(ProcReadError::Unavailable),
            501 => Ok(ProcIdentity {
                process_group: 500,
                start_ticks: 5_001,
            }),
            500 => Err(ProcReadError::Malformed),
            101 => Ok(ProcIdentity {
                process_group: 100,
                start_ticks: 1_001,
            }),
            _ => Err(ProcReadError::Unavailable),
        },
    )?;

    assert_eq!(
        correlations,
        vec![
            ManagedPidCorrelation {
                pid: 100,
                evidence: ManagedPidCorrelationEvidence::Leader(valid.clone()),
            },
            ManagedPidCorrelation {
                pid: 200,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
            ManagedPidCorrelation {
                pid: 201,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
            ManagedPidCorrelation {
                pid: 301,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
            ManagedPidCorrelation {
                pid: 401,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
            ManagedPidCorrelation {
                pid: 501,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            },
            ManagedPidCorrelation {
                pid: 101,
                evidence: ManagedPidCorrelationEvidence::ProcessGroup(valid),
            },
        ]
    );
    Ok(())
}
