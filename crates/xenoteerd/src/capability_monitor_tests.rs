use xenoteer_protocol::WindowManagerCapability;

use super::*;

fn capabilities(capability: WindowManagerCapability) -> RawWindowManagerCapabilities {
    RawWindowManagerCapabilities {
        supported: vec![capability],
        restack: false,
    }
}

#[test]
fn successful_probe_replaces_pending_or_stale_evidence() {
    let cache = RwLock::new(WindowCapabilitySnapshot {
        evidence_state: WindowCapabilityEvidenceState::Pending,
        capabilities: None,
    });
    let first = capabilities(WindowManagerCapability::Activate);
    apply_probe_result(&cache, Ok(first.clone()));
    assert_eq!(
        read_lock(&cache).clone(),
        WindowCapabilitySnapshot {
            evidence_state: WindowCapabilityEvidenceState::Current,
            capabilities: Some(first),
        }
    );

    apply_probe_result(&cache, Err(WindowCapabilityProbeFailure::Busy));
    assert_eq!(
        read_lock(&cache).evidence_state,
        WindowCapabilityEvidenceState::Stale
    );
    let second = capabilities(WindowManagerCapability::MoveResize);
    apply_probe_result(&cache, Ok(second.clone()));
    assert_eq!(read_lock(&cache).capabilities, Some(second));
    assert_eq!(
        read_lock(&cache).evidence_state,
        WindowCapabilityEvidenceState::Current
    );
}

#[test]
fn transient_failure_retains_only_an_existing_trustworthy_projection() {
    let cache = RwLock::new(WindowCapabilitySnapshot {
        evidence_state: WindowCapabilityEvidenceState::Pending,
        capabilities: None,
    });
    apply_probe_result(&cache, Err(WindowCapabilityProbeFailure::TimedOut));
    assert_eq!(
        read_lock(&cache).clone(),
        WindowCapabilitySnapshot {
            evidence_state: WindowCapabilityEvidenceState::Unavailable,
            capabilities: None,
        }
    );

    let prior = capabilities(WindowManagerCapability::StateFullscreen);
    apply_probe_result(&cache, Ok(prior.clone()));
    apply_probe_result(&cache, Err(WindowCapabilityProbeFailure::Rejected));
    assert_eq!(read_lock(&cache).capabilities, Some(prior));
    assert_eq!(
        read_lock(&cache).evidence_state,
        WindowCapabilityEvidenceState::Stale
    );
}

#[test]
fn terminal_failure_discards_old_backend_evidence() {
    let prior = capabilities(WindowManagerCapability::Close);
    let cache = RwLock::new(WindowCapabilitySnapshot {
        evidence_state: WindowCapabilityEvidenceState::Current,
        capabilities: Some(prior),
    });
    apply_probe_result(&cache, Err(WindowCapabilityProbeFailure::Terminal));
    assert_eq!(
        read_lock(&cache).clone(),
        WindowCapabilitySnapshot {
            evidence_state: WindowCapabilityEvidenceState::Unavailable,
            capabilities: None,
        }
    );
}

#[test]
fn operation_backend_probe_results_are_independent_and_recoverable() {
    let cache = RwLock::new(OperationBackendSnapshot {
        artifact: BackendCapabilityEvidenceState::Pending,
        process: BackendCapabilityEvidenceState::Pending,
    });

    apply_operation_probe_results(&cache, Ok(()), Err(BackendProbeFailure));
    assert_eq!(
        *read_lock(&cache),
        OperationBackendSnapshot {
            artifact: BackendCapabilityEvidenceState::Current,
            process: BackendCapabilityEvidenceState::Unavailable,
        }
    );

    apply_operation_probe_results(&cache, Err(BackendProbeFailure), Ok(()));
    assert_eq!(
        *read_lock(&cache),
        OperationBackendSnapshot {
            artifact: BackendCapabilityEvidenceState::Stale,
            process: BackendCapabilityEvidenceState::Current,
        }
    );

    apply_operation_probe_results(&cache, Ok(()), Ok(()));
    assert_eq!(
        *read_lock(&cache),
        OperationBackendSnapshot {
            artifact: BackendCapabilityEvidenceState::Current,
            process: BackendCapabilityEvidenceState::Current,
        }
    );
}

#[test]
fn repeated_transient_failure_keeps_prior_evidence_stale() {
    let mut state = BackendCapabilityEvidenceState::Current;
    apply_backend_probe_result(&mut state, Err(BackendProbeFailure));
    assert_eq!(state, BackendCapabilityEvidenceState::Stale);
    apply_backend_probe_result(&mut state, Err(BackendProbeFailure));
    assert_eq!(state, BackendCapabilityEvidenceState::Stale);
}
