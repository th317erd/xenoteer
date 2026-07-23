#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::x11::{
    ActivationRequestPath, GeometryQuietTracker, GeometryWaitState, WireRequest,
    activation_request_path, encode_activate, encode_close, encode_current_workspace,
    encode_minimize, encode_move_resize, encode_restack, encode_state, encode_wm_delete,
    encode_workspace, normalize_active_window, observation_is_partial, observation_satisfies,
};
use super::*;
use crate::observe::atoms::{KnownAtom, KnownAtoms};

const WAIT: Duration = Duration::from_secs(2);

#[test]
fn active_window_normalization_accepts_only_singleton_or_xfwm_timestamp_extension() {
    assert_eq!(normalize_active_window(None).unwrap(), None);
    assert_eq!(normalize_active_window(Some(vec![42])).unwrap(), Some(42));
    assert_eq!(
        normalize_active_window(Some(vec![42, 0])).unwrap(),
        Some(42)
    );
    assert_eq!(
        normalize_active_window(Some(vec![42, 123_456])).unwrap(),
        Some(42)
    );
    assert_eq!(normalize_active_window(Some(vec![0])).unwrap(), None);
    assert_eq!(normalize_active_window(Some(vec![0, 7])).unwrap(), None);
    assert_eq!(
        normalize_active_window(Some(vec![42, 0, 0])),
        Err(BackendFault::MalformedWindowManagerData)
    );
}

#[derive(Clone, Default)]
struct FakeState {
    inner: Arc<FakeStateInner>,
}

#[derive(Default)]
struct FakeStateInner {
    gate: (Mutex<bool>, Condvar),
    entered: (Mutex<bool>, Condvar),
    calls: Mutex<Vec<&'static str>>,
    results: Mutex<VecDeque<std::result::Result<RawWindowControlEvidence, BackendFault>>>,
    capabilities: Mutex<VecDeque<std::result::Result<RawWindowManagerCapabilities, BackendFault>>>,
    panic: Mutex<bool>,
}

struct FakeBackend {
    state: FakeState,
}

impl WindowControlBackend for FakeBackend {
    fn capabilities(&mut self) -> std::result::Result<RawWindowManagerCapabilities, BackendFault> {
        lock_mutex(&self.state.inner.calls).push("capabilities");
        lock_mutex(&self.state.inner.capabilities)
            .pop_front()
            .unwrap_or_else(|| {
                Ok(RawWindowManagerCapabilities {
                    supported: vec![xenoteer_protocol::WindowManagerCapability::Activate],
                    restack: false,
                })
            })
    }

    fn execute(
        &mut self,
        request: &RawWindowControlRequest,
    ) -> std::result::Result<RawWindowControlEvidence, BackendFault> {
        lock_mutex(&self.state.inner.calls).push("execute");
        *lock_mutex(&self.state.inner.entered.0) = true;
        self.state.inner.entered.1.notify_all();
        let mut blocked = lock_mutex(&self.state.inner.gate.0);
        while *blocked {
            blocked = self.state.inner.gate.1.wait(blocked).unwrap();
        }
        assert!(!*lock_mutex(&self.state.inner.panic), "fake backend panic");
        lock_mutex(&self.state.inner.results)
            .pop_front()
            .unwrap_or_else(|| {
                Ok(RawWindowControlEvidence {
                    requested: request.clone(),
                    outcome: RawWindowControlOutcome::Converged,
                    observed: RawWindowControlObservation::NotObserved,
                    capabilities: None,
                    warnings: Vec::new(),
                })
            })
    }
}

impl FakeState {
    fn block(&self) {
        *lock_mutex(&self.inner.gate.0) = true;
    }

    fn release(&self) {
        *lock_mutex(&self.inner.gate.0) = false;
        self.inner.gate.1.notify_all();
    }

    fn wait_entered(&self) {
        let deadline = Instant::now() + WAIT;
        let mut entered = lock_mutex(&self.inner.entered.0);
        while !*entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero());
            (entered, _) = self
                .inner
                .entered
                .1
                .wait_timeout(entered, remaining)
                .unwrap();
        }
    }
}

fn request(target: Window) -> RawWindowControlRequest {
    RawWindowControlRequest {
        target,
        operation: RawWindowControlOperation::Activate {
            timestamp: 44,
            switch_workspace: None,
            allow_set_input_focus: false,
        },
        timeout: Duration::from_millis(100),
    }
}

#[test]
fn capability_probe_is_bounded_read_only_work_on_the_owner_actor() {
    let state = FakeState::default();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    let capabilities = handle
        .try_capabilities()
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(
        capabilities.supported,
        vec![xenoteer_protocol::WindowManagerCapability::Activate]
    );
    assert!(!capabilities.restack);
    assert_eq!(lock_mutex(&state.inner.calls).as_slice(), ["capabilities"]);
    assert_eq!(handle.health().completed_requests, 1);
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn capability_probe_shares_the_bounded_fifo_with_effects() {
    let state = FakeState::default();
    state.block();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    let running = handle.try_submit(request(7), || Ok(())).unwrap();
    state.wait_entered();
    let queued_probe = handle.try_capabilities().unwrap();
    assert_eq!(
        handle.try_capabilities().unwrap_err(),
        WindowControlSubmitError::QueueFull
    );
    state.release();
    running.recv_timeout(WAIT).unwrap().unwrap();
    queued_probe.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(handle.health().completed_requests, 2);
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn malformed_capability_probe_is_typed_and_does_not_poison_commands() {
    let state = FakeState::default();
    lock_mutex(&state.inner.capabilities).push_back(Err(BackendFault::MalformedWindowManagerData));
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    assert_eq!(
        handle
            .try_capabilities()
            .unwrap()
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap_err()
            .kind,
        WindowControlActorFailureKind::MalformedWindowManagerData
    );
    handle
        .try_submit(request(7), || Ok(()))
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(handle.health().state, WindowControlActorState::Healthy);
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn capability_probe_backend_loss_poison_closes_admission() {
    let state = FakeState::default();
    lock_mutex(&state.inner.capabilities).push_back(Err(BackendFault::BackendUnavailable));
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    assert_eq!(
        handle
            .try_capabilities()
            .unwrap()
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap_err()
            .kind,
        WindowControlActorFailureKind::BackendUnavailable
    );
    assert_eq!(join.join(), WindowControlActorExit::Poisoned);
    assert_eq!(handle.health().state, WindowControlActorState::Poisoned);
    assert_eq!(
        handle.try_capabilities().unwrap_err(),
        WindowControlSubmitError::Closed
    );
}

#[test]
fn stale_revalidation_runs_before_backend_and_prevents_any_effect() {
    let state = FakeState::default();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(2, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    let reply = handle
        .try_submit(request(7), || {
            Err(RawWindowRevalidationError::StaleReference)
        })
        .unwrap();
    assert_eq!(
        reply.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::StaleReference
    );
    assert!(lock_mutex(&state.inner.calls).is_empty());
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn revalidation_is_immediately_before_backend_execution() {
    let state = FakeState::default();
    let ordered = state.clone();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(2, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();
    handle
        .try_submit(request(7), move || {
            lock_mutex(&ordered.inner.calls).push("revalidate");
            Ok(())
        })
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(
        lock_mutex(&state.inner.calls).as_slice(),
        ["revalidate", "execute"]
    );
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn queued_cancellation_revalidation_prevents_a_late_backend_effect() {
    let state = FakeState::default();
    state.block();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(2, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    let running = handle.try_submit(request(7), || Ok(())).unwrap();
    state.wait_entered();
    let cancelled = Arc::new(AtomicBool::new(false));
    let queue_head_cancelled = Arc::clone(&cancelled);
    let queued = handle
        .try_submit(request(8), move || {
            if queue_head_cancelled.load(Ordering::Acquire) {
                Err(RawWindowRevalidationError::Rejected)
            } else {
                Ok(())
            }
        })
        .unwrap();
    cancelled.store(true, Ordering::Release);
    state.release();

    running.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::RevalidationRejected
    );
    assert_eq!(lock_mutex(&state.inner.calls).as_slice(), ["execute"]);
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn queued_deadline_revalidation_prevents_a_late_backend_effect() {
    let state = FakeState::default();
    state.block();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(2, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();

    let running = handle.try_submit(request(7), || Ok(())).unwrap();
    state.wait_entered();
    let deadline = Instant::now();
    let queued = handle
        .try_submit(request(8), move || {
            if Instant::now() >= deadline {
                Err(RawWindowRevalidationError::Rejected)
            } else {
                Ok(())
            }
        })
        .unwrap();
    state.release();

    running.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::RevalidationRejected
    );
    assert_eq!(lock_mutex(&state.inner.calls).as_slice(), ["execute"]);
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn queue_saturation_and_shutdown_are_bounded_and_reject_queued_effects() {
    let state = FakeState::default();
    state.block();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();
    let running = handle.try_submit(request(1), || Ok(())).unwrap();
    state.wait_entered();
    let queued = handle.try_submit(request(2), || Ok(())).unwrap();
    assert_eq!(
        handle.try_submit(request(3), || Ok(())).unwrap_err(),
        WindowControlSubmitError::QueueFull
    );
    let shutdown = handle.shutdown();
    state.release();
    running.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::ActorStopped
    );
    shutdown.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}

#[test]
fn backend_panic_is_contained_and_poison_closes_admission() {
    let state = FakeState::default();
    *lock_mutex(&state.inner.panic) = true;
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();
    let reply = handle.try_submit(request(1), || Ok(())).unwrap();
    assert!(reply.recv_timeout(WAIT).is_err());
    assert_eq!(join.join(), WindowControlActorExit::Panicked);
    assert_eq!(handle.health().state, WindowControlActorState::Panicked);
    assert_eq!(
        handle.try_submit(request(2), || Ok(())).unwrap_err(),
        WindowControlSubmitError::Closed
    );
}

#[test]
fn terminal_backend_failure_poison_rejects_queued_requests() {
    let state = FakeState::default();
    state.block();
    lock_mutex(&state.inner.results).push_back(Err(BackendFault::BackendUnavailable));
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();
    let failed = handle.try_submit(request(1), || Ok(())).unwrap();
    state.wait_entered();
    let queued = handle.try_submit(request(2), || Ok(())).unwrap();
    state.release();

    assert_eq!(
        failed.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::BackendUnavailable
    );
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        WindowControlActorFailureKind::ActorPoisoned
    );
    assert_eq!(join.join(), WindowControlActorExit::Poisoned);
    assert_eq!(handle.health().state, WindowControlActorState::Poisoned);
}

#[test]
fn fake_backend_preserves_timeout_and_partial_outcomes() {
    for outcome in [
        RawWindowControlOutcome::TimedOut,
        RawWindowControlOutcome::Partial,
    ] {
        let state = FakeState::default();
        lock_mutex(&state.inner.results).push_back(Ok(RawWindowControlEvidence {
            requested: request(4),
            outcome,
            observed: RawWindowControlObservation::NotObserved,
            capabilities: None,
            warnings: Vec::new(),
        }));
        let factory_state = state.clone();
        let (handle, join) = spawn_with_backend(1, move || {
            Ok(FakeBackend {
                state: factory_state,
            })
        })
        .unwrap();
        assert_eq!(
            handle
                .try_submit(request(4), || Ok(()))
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap()
                .outcome,
            outcome
        );
        assert_eq!(join.join(), WindowControlActorExit::Stopped);
    }
}

#[test]
fn malformed_and_semantic_backend_results_remain_typed_evidence() {
    for (fault, expected) in [
        (
            BackendFault::Unsupported,
            RawWindowControlOutcome::Unsupported,
        ),
        (
            BackendFault::MalformedWindowManagerData,
            RawWindowControlOutcome::MalformedWindowManagerData,
        ),
        (BackendFault::Refused, RawWindowControlOutcome::Refused),
        (
            BackendFault::TargetVanished,
            RawWindowControlOutcome::TargetVanished,
        ),
    ] {
        let state = FakeState::default();
        lock_mutex(&state.inner.results).push_back(Err(fault));
        let factory_state = state.clone();
        let (handle, join) = spawn_with_backend(1, move || {
            Ok(FakeBackend {
                state: factory_state,
            })
        })
        .unwrap();
        let evidence = handle
            .try_submit(request(9), || Ok(()))
            .unwrap()
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.outcome, expected);
        assert_eq!(join.join(), WindowControlActorExit::Stopped);
    }
}

#[test]
fn ewmh_and_icccm_messages_use_exact_layouts_and_destinations() {
    let root = 1;
    let target = 20;
    let message_type = 300;
    assert_root_message(
        encode_activate(root, target, message_type, 90, Some(19)),
        root,
        target,
        message_type,
        [2, 90, 19, 0, 0],
    );
    assert_root_message(
        encode_close(root, target, message_type, 91),
        root,
        target,
        message_type,
        [91, 2, 0, 0, 0],
    );
    assert_root_message(
        encode_minimize(root, target, message_type),
        root,
        target,
        message_type,
        [3, 0, 0, 0, 0],
    );
    assert_eq!(
        encode_wm_delete(target, 400, 401, 92),
        WireRequest::ClientMessage {
            destination: target,
            event_mask: x11rb::protocol::xproto::EventMask::NO_EVENT,
            window: target,
            message_type: 400,
            data: [401, 92, 0, 0, 0],
        }
    );
}

#[test]
fn set_input_focus_is_never_selected_without_explicit_opt_in() {
    assert_eq!(
        activation_request_path(true, false),
        ActivationRequestPath::Ewmh
    );
    assert_eq!(
        activation_request_path(true, true),
        ActivationRequestPath::Ewmh
    );
    assert_eq!(
        activation_request_path(false, false),
        ActivationRequestPath::Unsupported
    );
    assert_eq!(
        activation_request_path(false, true),
        ActivationRequestPath::SetInputFocus
    );
}

#[test]
fn desired_state_encoding_never_uses_toggle_and_maximize_is_compound() {
    let atoms = KnownAtoms::for_test(|atom| atom as u32 + 1_000);
    for desired in [false, true] {
        let wire = encode_state(
            1,
            2,
            atoms.get(KnownAtom::NetWmState),
            xenoteer_protocol::WindowManagerState::Maximized,
            desired,
            &atoms,
        );
        assert_root_message(
            wire,
            1,
            2,
            atoms.get(KnownAtom::NetWmState),
            [
                u32::from(desired),
                atoms.get(KnownAtom::NetWmStateMaximizedVert),
                atoms.get(KnownAtom::NetWmStateMaximizedHorz),
                2,
                0,
            ],
        );
    }
}

#[test]
fn moveresize_workspace_and_restack_flags_match_ewmh() {
    let geometry = WindowGeometryRequest {
        x: Some(-12),
        y: None,
        width: Some(640),
        height: Some(480),
    };
    assert_root_message(
        encode_move_resize(1, 2, 3, geometry),
        1,
        2,
        3,
        [
            10 | (1 << 8) | (1 << 10) | (1 << 11) | (2 << 12),
            (-12_i32) as u32,
            0,
            640,
            480,
        ],
    );
    assert_root_message(encode_workspace(1, 2, 3, 4), 1, 2, 3, [4, 2, 0, 0, 0]);
    assert_root_message(
        encode_current_workspace(1, 3, 4, 99),
        1,
        1,
        3,
        [4, 99, 0, 0, 0],
    );
    assert_root_message(
        encode_restack(1, 2, 3, WindowStackMode::Below, Some(9)),
        1,
        2,
        3,
        [2, 9, 1, 0, 0],
    );
}

#[test]
fn geometry_quiet_window_restarts_after_intermediate_matching_configure() {
    let mut tracker = GeometryQuietTracker::new();
    assert_eq!(
        tracker.observe(Duration::ZERO, false, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_millis(49), false, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_millis(50), false, true),
        GeometryWaitState::Settled
    );

    let mut tracker = GeometryQuietTracker::new();
    assert_eq!(
        tracker.observe(Duration::from_millis(40), false, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_millis(45), true, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_millis(94), false, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_millis(95), false, true),
        GeometryWaitState::Settled
    );
}

#[test]
fn geometry_quiet_window_is_bounded_at_one_second() {
    let mut tracker = GeometryQuietTracker::new();
    assert_eq!(
        tracker.observe(Duration::from_millis(980), true, true),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        tracker.observe(Duration::from_secs(1), false, true),
        GeometryWaitState::Expired
    );

    let mut never_matching = GeometryQuietTracker::new();
    assert_eq!(
        never_matching.observe(Duration::from_secs(1), false, false),
        GeometryWaitState::Expired
    );
}

#[test]
fn unchanged_mismatch_waits_but_changed_constraint_settles_after_quiet() {
    let mut ignored = GeometryQuietTracker::new();
    assert_eq!(
        ignored.observe(Duration::from_millis(50), false, false),
        GeometryWaitState::Waiting
    );

    let mut constrained = GeometryQuietTracker::new();
    assert_eq!(
        constrained.observe(Duration::from_millis(10), true, false),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        constrained.observe(Duration::from_millis(59), false, false),
        GeometryWaitState::Waiting
    );
    assert_eq!(
        constrained.observe(Duration::from_millis(60), false, false),
        GeometryWaitState::Settled
    );
}

#[test]
fn convergence_classifies_partial_evidence_without_false_success() {
    let state_request = RawWindowControlRequest {
        target: 2,
        operation: RawWindowControlOperation::SetState {
            state: xenoteer_protocol::WindowManagerState::Maximized,
            desired: true,
        },
        timeout: Duration::from_millis(10),
    };
    let disabled = RawWindowControlObservation::State(RawWindowBooleanObservation::Disabled);
    let partial = RawWindowControlObservation::State(RawWindowBooleanObservation::Partial);
    assert!(!observation_satisfies(&state_request, &partial));
    assert!(observation_is_partial(&state_request, &disabled, &partial));

    let activate = request(2);
    let unchanged = RawWindowControlObservation::Activation {
        current_active_sent: Some(8),
        timestamp_sent: 44,
        active: Some(8),
        focused: Some(8),
        focus_within_target: false,
        focus_ancestry_status: crate::FocusAncestryStatus::Resolved,
        current_workspace: Some(0),
    };
    assert!(!observation_satisfies(&activate, &unchanged));
    assert!(!observation_is_partial(&activate, &unchanged, &unchanged));

    let descendant_focus = RawWindowControlObservation::Activation {
        current_active_sent: Some(8),
        timestamp_sent: 44,
        active: Some(2),
        focused: Some(40),
        focus_within_target: true,
        focus_ancestry_status: crate::FocusAncestryStatus::Resolved,
        current_workspace: Some(0),
    };
    assert!(observation_satisfies(&activate, &descendant_focus));
}

fn assert_root_message(
    wire: WireRequest,
    root: Window,
    window: Window,
    message_type: u32,
    data: [u32; 5],
) {
    assert_eq!(
        wire,
        WireRequest::ClientMessage {
            destination: root,
            event_mask: x11rb::protocol::xproto::EventMask::SUBSTRUCTURE_NOTIFY
                | x11rb::protocol::xproto::EventMask::SUBSTRUCTURE_REDIRECT,
            window,
            message_type,
            data,
        }
    );
}

#[test]
fn malformed_requests_are_rejected_before_admission() {
    let state = FakeState::default();
    let factory_state = state.clone();
    let (handle, join) = spawn_with_backend(1, move || {
        Ok(FakeBackend {
            state: factory_state,
        })
    })
    .unwrap();
    assert_eq!(
        handle.try_submit(request(0), || Ok(())).unwrap_err(),
        WindowControlSubmitError::InvalidRequest(RawWindowControlRequestError::InvalidTarget)
    );
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
}
