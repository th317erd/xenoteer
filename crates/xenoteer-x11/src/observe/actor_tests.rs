// These are time-bounded assertion tests; an unexpected result should fail at
// the exact assertion site instead of being converted into actor behavior.
#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use xenoteer_protocol::{CoordinateSpace, Rect, WindowMapState, WindowRect};

use super::actor::{
    MAX_EVENTS_PER_TURN, ObservationBackend, ObservationBackendFault, spawn_with_backend,
};
use super::atoms::KnownAtoms;
use super::{
    InventorySource, ObservationActorEvent, ObservationActorExit, ObservationActorFailureKind,
    ObservationActorState, ObservationActorSubmitError, PollThreadEvent, RootDamageCoverage,
    RootDamageHint, RootDamageRect, RootGeometryInput, RootInventory, WindowAttributeInput,
    WindowPropertyInput, WindowSnapshotInput,
};

const WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
struct MockState {
    inner: Arc<MockStateInner>,
}

#[derive(Default)]
struct MockStateInner {
    gate: (Mutex<GateState>, Condvar),
    event_poll_gate: (Mutex<GateState>, Condvar),
    events: Mutex<VecDeque<PollThreadEvent>>,
    event_progress: Condvar,
    calls: Mutex<Vec<ThreadId>>,
    factory_thread: Mutex<Option<ThreadId>>,
    drop_thread: Mutex<Option<ThreadId>>,
    fail_snapshot_terminal: Mutex<bool>,
    fail_snapshot_nonterminal: Mutex<bool>,
    poll_faults: Mutex<VecDeque<ObservationBackendFault>>,
    observe_window_fault: Mutex<Option<ObservationBackendFault>>,
}

#[derive(Default)]
struct GateState {
    blocked: bool,
    entered: bool,
    released: bool,
}

impl MockState {
    fn block_snapshot(&self) {
        lock(&self.inner.gate.0).blocked = true;
    }

    fn wait_snapshot_entered(&self) {
        let deadline = Instant::now() + WAIT;
        let mut state = lock(&self.inner.gate.0);
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "snapshot gate was never entered");
            let (next, timeout) = self.inner.gate.1.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timeout.timed_out() || state.entered);
        }
    }

    fn release_snapshot(&self) {
        let mut state = lock(&self.inner.gate.0);
        state.released = true;
        self.inner.gate.1.notify_all();
    }

    fn block_event_poll(&self) {
        lock(&self.inner.event_poll_gate.0).blocked = true;
    }

    fn wait_event_poll_entered(&self) {
        let deadline = Instant::now() + WAIT;
        let mut state = lock(&self.inner.event_poll_gate.0);
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "event poll gate was never entered");
            let (next, timeout) = self
                .inner
                .event_poll_gate
                .1
                .wait_timeout(state, remaining)
                .unwrap();
            state = next;
            assert!(!timeout.timed_out() || state.entered);
        }
    }

    fn release_event_poll(&self) {
        let mut state = lock(&self.inner.event_poll_gate.0);
        state.released = true;
        self.inner.event_poll_gate.1.notify_all();
    }

    fn enqueue_events(&self, count: u32) {
        let mut events = lock(&self.inner.events);
        for window in 10..10 + count {
            events.push_back(PollThreadEvent::Create { window });
        }
    }

    fn enqueue_event(&self, event: PollThreadEvent) {
        lock(&self.inner.events).push_back(event);
    }

    fn fail_next_observe_window(&self, fault: ObservationBackendFault) {
        *lock(&self.inner.observe_window_fault) = Some(fault);
    }

    fn enqueue_poll_fault(&self, fault: ObservationBackendFault) {
        lock(&self.inner.poll_faults).push_back(fault);
    }

    fn wait_events_drained(&self) {
        let deadline = Instant::now() + WAIT;
        let mut events = lock(&self.inner.events);
        while !events.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "mock events were not drained");
            let (next, timeout) = self
                .inner
                .event_progress
                .wait_timeout(events, remaining)
                .unwrap();
            events = next;
            assert!(!timeout.timed_out() || events.is_empty());
        }
    }

    fn wait_dropped(&self) -> ThreadId {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(thread) = *lock(&self.inner.drop_thread) {
                return thread;
            }
            assert!(Instant::now() < deadline, "backend was not dropped");
            thread::yield_now();
        }
    }
}

struct MockBackend {
    state: MockState,
    atoms: KnownAtoms,
}

impl MockBackend {
    fn new(state: MockState) -> Self {
        *lock(&state.inner.factory_thread) = Some(thread::current().id());
        Self {
            state,
            atoms: KnownAtoms::for_test(|atom| atom as u32 + 100),
        }
    }

    fn record_call(&self) {
        lock(&self.state.inner.calls).push(thread::current().id());
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        *lock(&self.state.inner.drop_thread) = Some(thread::current().id());
    }
}

impl ObservationBackend for MockBackend {
    fn root(&self) -> u32 {
        1
    }

    fn atoms(&self) -> &KnownAtoms {
        &self.atoms
    }

    fn snapshot(&mut self, window: u32) -> Result<WindowSnapshotInput, ObservationBackendFault> {
        self.record_call();
        {
            let mut gate = lock(&self.state.inner.gate.0);
            if gate.blocked {
                gate.entered = true;
                self.state.inner.gate.1.notify_all();
                while !gate.released {
                    gate = self.state.inner.gate.1.wait(gate).unwrap();
                }
            }
        }
        if *lock(&self.state.inner.fail_snapshot_terminal) {
            return Err(ObservationBackendFault::terminal(
                ObservationActorFailureKind::BackendUnavailable,
            ));
        }
        if *lock(&self.state.inner.fail_snapshot_nonterminal) {
            return Err(ObservationBackendFault::request_failed());
        }
        Ok(snapshot(window))
    }

    fn reconcile(&mut self) -> Result<RootInventory, ObservationBackendFault> {
        self.record_call();
        Ok(RootInventory {
            windows: vec![10, 20],
            source: InventorySource::NetClientList,
            warnings: Vec::new(),
        })
    }

    fn health_check(&mut self) -> Result<(), ObservationBackendFault> {
        self.record_call();
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<PollThreadEvent>, ObservationBackendFault> {
        if let Some(fault) = lock(&self.state.inner.poll_faults).pop_front() {
            self.record_call();
            return Err(fault);
        }
        let event = lock(&self.state.inner.events).pop_front();
        if event.is_some() {
            let mut gate = lock(&self.state.inner.event_poll_gate.0);
            if gate.blocked {
                gate.entered = true;
                self.state.inner.event_poll_gate.1.notify_all();
                while !gate.released {
                    gate = self.state.inner.event_poll_gate.1.wait(gate).unwrap();
                }
            }
            self.record_call();
            self.state.inner.event_progress.notify_all();
        }
        Ok(event)
    }

    fn observe_window(&mut self, _window: u32) -> Result<(), ObservationBackendFault> {
        self.record_call();
        if let Some(fault) = lock(&self.state.inner.observe_window_fault).take() {
            return Err(fault);
        }
        Ok(())
    }
}

#[test]
fn ordinary_lane_is_bounded_and_supports_snapshot_reconcile_health_and_shutdown() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(1, 4, move || Ok(MockBackend::new(factory_state))).unwrap();

    let snapshot = handle.try_snapshot(55).unwrap();
    state.wait_snapshot_entered();
    let reconcile = handle.try_reconcile().unwrap();
    assert_eq!(
        handle.try_health_check().unwrap_err(),
        ObservationActorSubmitError::QueueFull
    );
    state.release_snapshot();

    assert_eq!(snapshot.recv_timeout(WAIT).unwrap().unwrap().window, 55);
    assert_eq!(
        reconcile.recv_timeout(WAIT).unwrap().unwrap().windows,
        vec![10, 20]
    );
    let health = handle
        .try_health_check()
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(health.state, ObservationActorState::Healthy);
    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn snapshot_reply_follows_an_event_buffered_during_the_snapshot_round_trip() {
    let state = MockState::default();
    state.block_snapshot();
    state.block_event_poll();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 4, move || Ok(MockBackend::new(factory_state))).unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_event(PollThreadEvent::Destroy { window: 55 });
    state.release_snapshot();
    state.wait_event_poll_entered();
    let barrier_completed = reply.try_recv();
    state.release_event_poll();
    assert!(
        barrier_completed.is_err(),
        "snapshot barrier completed before its ordered event was emitted"
    );

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    let snapshot_window = match &barrier.outcome {
        super::ObservationSnapshotOutcome::Snapshot(snapshot) => Some(snapshot.window),
        super::ObservationSnapshotOutcome::RequestFailed => None,
    };
    assert_eq!(snapshot_window, Some(55));
    assert!(barrier.stable);
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile {
            decision: super::ReconcileDecision::RemoveWindow { window: 55 },
        }
    );

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn failed_snapshot_barrier_still_follows_its_ordered_event() {
    let state = MockState::default();
    state.block_snapshot();
    state.block_event_poll();
    *lock(&state.inner.fail_snapshot_nonterminal) = true;
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 4, move || Ok(MockBackend::new(factory_state))).unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_event(PollThreadEvent::Destroy { window: 55 });
    state.release_snapshot();
    state.wait_event_poll_entered();
    let barrier_completed = reply.try_recv();
    state.release_event_poll();
    assert!(
        barrier_completed.is_err(),
        "failed snapshot barrier completed before its ordered event was emitted"
    );

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    assert!(matches!(
        barrier.outcome,
        super::ObservationSnapshotOutcome::RequestFailed
    ));
    assert!(barrier.stable);
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile {
            decision: super::ReconcileDecision::RemoveWindow { window: 55 },
        }
    );

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn snapshot_barrier_reports_unstable_when_its_event_budget_is_exhausted() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, _events, join) = spawn_with_backend(2, MAX_EVENTS_PER_TURN * 2, move || {
        Ok(MockBackend::new(factory_state))
    })
    .unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_events(u32::try_from(MAX_EVENTS_PER_TURN).unwrap());
    state.release_snapshot();

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    let snapshot_window = match &barrier.outcome {
        super::ObservationSnapshotOutcome::Snapshot(snapshot) => Some(snapshot.window),
        super::ObservationSnapshotOutcome::RequestFailed => None,
    };
    assert_eq!(snapshot_window, Some(55));
    assert!(!barrier.stable);

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn snapshot_barrier_reports_unstable_while_event_overflow_is_latched() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 1, move || Ok(MockBackend::new(factory_state))).unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_events(2);
    state.release_snapshot();

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    let snapshot_window = match &barrier.outcome {
        super::ObservationSnapshotOutcome::Snapshot(snapshot) => Some(snapshot.window),
        super::ObservationSnapshotOutcome::RequestFailed => None,
    };
    assert_eq!(snapshot_window, Some(55));
    assert!(!barrier.stable);
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile { .. }
    ));
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::ResyncRequired
    );

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn terminal_snapshot_fault_through_snapshot_barrier_poison_closes_the_actor() {
    let state = MockState::default();
    state.block_snapshot();
    *lock(&state.inner.fail_snapshot_terminal) = true;
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    let barrier = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    let queued = handle.try_health_check().unwrap();
    state.release_snapshot();

    assert_eq!(
        barrier.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::BackendUnavailable
    );
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::ActorPoisoned
    );
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed {
            failure: super::ObservationActorFailure {
                kind: ObservationActorFailureKind::BackendUnavailable
            }
        }
    ));
    assert_eq!(handle.health().state, ObservationActorState::Poisoned);
    assert_eq!(
        handle.try_snapshot_barrier(56).unwrap_err(),
        ObservationActorSubmitError::Closed
    );
    assert_eq!(join.join(), ObservationActorExit::Poisoned);
}

#[test]
fn terminal_event_fault_during_snapshot_barrier_drain_poison_closes_the_actor() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    let barrier = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.fail_next_observe_window(ObservationBackendFault::terminal(
        ObservationActorFailureKind::BackendUnavailable,
    ));
    state.enqueue_event(PollThreadEvent::Create { window: 55 });
    state.release_snapshot();

    assert_eq!(
        barrier.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::BackendUnavailable
    );
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed {
            failure: super::ObservationActorFailure {
                kind: ObservationActorFailureKind::BackendUnavailable
            }
        }
    ));
    assert_eq!(handle.health().state, ObservationActorState::Poisoned);
    assert_eq!(join.join(), ObservationActorExit::Poisoned);
}

#[test]
fn terminal_poll_fault_during_snapshot_barrier_drain_poison_closes_the_actor() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    let barrier = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_poll_fault(ObservationBackendFault::terminal(
        ObservationActorFailureKind::BackendUnavailable,
    ));
    state.release_snapshot();

    assert_eq!(
        barrier.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::BackendUnavailable
    );
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed {
            failure: super::ObservationActorFailure {
                kind: ObservationActorFailureKind::BackendUnavailable
            }
        }
    ));
    assert_eq!(handle.health().state, ObservationActorState::Poisoned);
    assert_eq!(join.join(), ObservationActorExit::Poisoned);
}

#[test]
fn nonterminal_poll_fault_during_snapshot_barrier_drain_is_explicit_and_unstable() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_poll_fault(ObservationBackendFault::request_failed());
    state.release_snapshot();

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    let snapshot_window = match &barrier.outcome {
        super::ObservationSnapshotOutcome::Snapshot(snapshot) => Some(snapshot.window),
        super::ObservationSnapshotOutcome::RequestFailed => None,
    };
    assert_eq!(snapshot_window, Some(55));
    assert!(!barrier.stable);
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed {
            failure: super::ObservationActorFailure {
                kind: ObservationActorFailureKind::RequestFailed
            }
        }
    ));
    assert_eq!(handle.health().state, ObservationActorState::Healthy);

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn queued_snapshot_barrier_is_rejected_by_orderly_shutdown() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    let running = handle.try_snapshot(10).unwrap();
    state.wait_snapshot_entered();
    let queued_barrier = handle.try_snapshot_barrier(11).unwrap();
    let shutdown = handle.shutdown();
    assert_eq!(
        handle.try_snapshot_barrier(12).unwrap_err(),
        ObservationActorSubmitError::Closed
    );
    state.release_snapshot();

    assert_eq!(running.recv_timeout(WAIT).unwrap().unwrap().window, 10);
    assert_eq!(
        queued_barrier.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::ActorStopped
    );
    shutdown.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn root_damage_during_snapshot_barrier_drain_is_coalesced_without_instability() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 4, move || Ok(MockBackend::new(factory_state))).unwrap();
    let root_region = RootDamageRect::new(0, 0, 800, 600).unwrap();

    let reply = handle.try_snapshot_barrier(55).unwrap();
    state.wait_snapshot_entered();
    state.enqueue_event(PollThreadEvent::RootDamage {
        hint: RootDamageHint {
            area: RootDamageRect::new(10, 20, 30, 40).unwrap(),
            root_region,
        },
    });
    state.enqueue_event(PollThreadEvent::RootDamage {
        hint: RootDamageHint {
            area: RootDamageRect::new(20, 30, 30, 40).unwrap(),
            root_region,
        },
    });
    state.release_snapshot();

    let barrier = reply.recv_timeout(WAIT).unwrap().unwrap();
    assert!(barrier.stable);
    let damage_event = events.recv_timeout(WAIT).unwrap();
    assert!(matches!(
        damage_event,
        ObservationActorEvent::RootDamaged { .. }
    ));
    let ObservationActorEvent::RootDamaged { damage } = damage_event else {
        return;
    };
    assert_eq!(damage.root_region, root_region);
    assert_eq!(
        damage.regions,
        vec![RootDamageRect::new(10, 20, 40, 50).unwrap()]
    );
    assert_eq!(damage.coverage, RootDamageCoverage::Regions);
    assert_eq!(damage.notifications, 2);
    assert!(events.recv_timeout(Duration::from_millis(50)).is_err());

    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn backend_is_created_used_and_dropped_on_exactly_one_worker_thread() {
    let caller = thread::current().id();
    let state = MockState::default();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(2, 2, move || Ok(MockBackend::new(factory_state))).unwrap();
    handle
        .try_snapshot(77)
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    handle
        .try_health_check()
        .unwrap()
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);

    let factory = (*lock(&state.inner.factory_thread)).unwrap();
    let drop_thread = state.wait_dropped();
    let calls = lock(&state.inner.calls).clone();
    assert_ne!(factory, caller);
    assert_eq!(drop_thread, factory);
    assert!(!calls.is_empty());
    assert!(calls.into_iter().all(|thread| thread == factory));
}

#[test]
fn shutdown_bypasses_a_saturated_ordinary_lane_and_rejects_queued_work() {
    let state = MockState::default();
    state.block_snapshot();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();
    let running = handle.try_snapshot(10).unwrap();
    state.wait_snapshot_entered();
    let queued = handle.try_snapshot(11).unwrap();
    let shutdown = handle.shutdown();
    let coalesced_shutdown = handle.shutdown();
    assert_eq!(
        handle.try_snapshot(12).unwrap_err(),
        ObservationActorSubmitError::Closed
    );

    let released = Instant::now();
    state.release_snapshot();
    running.recv_timeout(WAIT).unwrap().unwrap();
    let failure = queued.recv_timeout(WAIT).unwrap().unwrap_err();
    assert_eq!(failure.kind, ObservationActorFailureKind::ActorStopped);
    shutdown.recv_timeout(WAIT).unwrap().unwrap();
    coalesced_shutdown.recv_timeout(WAIT).unwrap().unwrap();
    assert!(released.elapsed() < Duration::from_millis(500));
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn event_overflow_emits_one_nonblocking_coalesced_resync_without_debounce() {
    let state = MockState::default();
    state.enqueue_events(16);
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 1, move || Ok(MockBackend::new(factory_state))).unwrap();
    state.wait_events_drained();

    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile { .. }
    ));
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::ResyncRequired
    );
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn event_overflow_after_damage_emits_resync_then_full_damage_marker() {
    let state = MockState::default();
    let root_region = RootDamageRect::new(0, 0, 800, 600).unwrap();
    state.enqueue_event(PollThreadEvent::RootDamage {
        hint: RootDamageHint {
            area: RootDamageRect::new(10, 20, 30, 40).unwrap(),
            root_region,
        },
    });
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(2, 1, move || Ok(MockBackend::new(factory_state))).unwrap();
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::RootDamaged { .. }
    ));

    state.enqueue_events(16);
    state.wait_events_drained();
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile { .. }
    ));
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::ResyncRequired
    );
    let marker = events.recv_timeout(WAIT).unwrap();
    assert!(matches!(marker, ObservationActorEvent::RootDamaged { .. }));
    let ObservationActorEvent::RootDamaged { damage } = marker else {
        return;
    };
    assert_eq!(damage.root_region, root_region);
    assert_eq!(damage.regions, vec![root_region]);
    assert_eq!(damage.coverage, RootDamageCoverage::FullScreen);
    assert_eq!(damage.notifications, 1);
    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn continuity_preserving_root_resync_keeps_its_classified_cause() {
    let state = MockState::default();
    state.enqueue_event(PollThreadEvent::Configure {
        window: 1,
        above_sibling: 0,
    });
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Reconcile {
            decision: super::ReconcileDecision::FullResync,
        }
    );
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(join.join(), ObservationActorExit::Stopped);
    assert_eq!(handle.health().state, ObservationActorState::Stopped);
}

#[test]
fn explicit_upstream_event_loss_remains_a_true_discontinuity() {
    let state = MockState::default();
    state.enqueue_event(PollThreadEvent::ResyncRequired);
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::ResyncRequired
    );
    assert_eq!(join.join(), ObservationActorExit::Stopped);
    assert_eq!(handle.health().state, ObservationActorState::Stopped);
}

#[test]
fn terminal_backend_failure_poison_closes_admission_and_join_reports_poisoned() {
    let state = MockState::default();
    state.block_snapshot();
    *lock(&state.inner.fail_snapshot_terminal) = true;
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();
    let failed = handle.try_snapshot(10).unwrap();
    state.wait_snapshot_entered();
    let queued = handle.try_reconcile().unwrap();
    state.release_snapshot();

    assert_eq!(
        failed.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::BackendUnavailable
    );
    assert_eq!(
        queued.recv_timeout(WAIT).unwrap().unwrap_err().kind,
        ObservationActorFailureKind::ActorPoisoned
    );
    assert_eq!(handle.health().state, ObservationActorState::Poisoned);
    assert_eq!(
        handle.try_snapshot(11).unwrap_err(),
        ObservationActorSubmitError::Closed
    );
    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed { .. }
    ));
    assert_eq!(join.join(), ObservationActorExit::Poisoned);
}

#[test]
fn nonterminal_snapshot_read_failure_is_an_explicit_error_not_false_completion() {
    let state = MockState::default();
    *lock(&state.inner.fail_snapshot_nonterminal) = true;
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    assert_eq!(
        handle
            .try_snapshot(10)
            .unwrap()
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap_err()
            .kind,
        ObservationActorFailureKind::RequestFailed
    );
    assert_eq!(handle.health().state, ObservationActorState::Healthy);
    assert_eq!(handle.health().completed_requests, 0);
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn dropping_join_requests_shutdown_and_never_detaches_the_worker() {
    let state = MockState::default();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(1, 1, move || Ok(MockBackend::new(factory_state))).unwrap();
    drop(join);
    state.wait_dropped();
    assert_eq!(handle.health().state, ObservationActorState::Stopped);
    assert_eq!(
        handle.try_reconcile().unwrap_err(),
        ObservationActorSubmitError::Closed
    );
}

#[test]
fn join_requests_shutdown_even_while_an_admission_handle_is_live() {
    let state = MockState::default();
    let factory_state = state.clone();
    let (handle, _events, join) =
        spawn_with_backend(1, 1, move || Ok(MockBackend::new(factory_state))).unwrap();

    let started = Instant::now();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(handle.health().state, ObservationActorState::Stopped);
    assert_eq!(
        handle.try_reconcile().unwrap_err(),
        ObservationActorSubmitError::Closed
    );
}

#[test]
fn checked_window_subscription_failure_forces_resync_instead_of_incremental_output() {
    let state = MockState::default();
    state.fail_next_observe_window(ObservationBackendFault::request_failed());
    state.enqueue_events(1);
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(1, 4, move || Ok(MockBackend::new(factory_state))).unwrap();

    let event = events.recv_timeout(WAIT).unwrap();
    assert!(matches!(event, ObservationActorEvent::Failed { .. }));
    let ObservationActorEvent::Failed { failure } = event else {
        return;
    };
    assert_eq!(failure.kind, ObservationActorFailureKind::RequestFailed);
    assert_eq!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::ResyncRequired
    );
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    handle.shutdown().recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(join.join(), ObservationActorExit::Stopped);
}

#[test]
fn terminal_window_subscription_failure_poison_stops_the_actor() {
    let state = MockState::default();
    state.fail_next_observe_window(ObservationBackendFault::terminal(
        ObservationActorFailureKind::BackendUnavailable,
    ));
    state.enqueue_events(1);
    let factory_state = state.clone();
    let (handle, events, join) =
        spawn_with_backend(1, 2, move || Ok(MockBackend::new(factory_state))).unwrap();

    assert!(matches!(
        events.recv_timeout(WAIT).unwrap(),
        ObservationActorEvent::Failed {
            failure: super::ObservationActorFailure {
                kind: ObservationActorFailureKind::BackendUnavailable
            }
        }
    ));
    assert_eq!(join.join(), ObservationActorExit::Poisoned);
    assert_eq!(handle.health().state, ObservationActorState::Poisoned);
}

fn snapshot(window: u32) -> WindowSnapshotInput {
    let rect = Rect::new(0, 0, 1, 1).unwrap();
    WindowSnapshotInput {
        window,
        attributes: WindowAttributeInput {
            map_state: WindowMapState::Viewable,
            override_redirect: false,
            input_only: false,
            visual: 1,
            colormap: 1,
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
            client_rect: WindowRect::new(CoordinateSpace::RootPhysical, rect).unwrap(),
            border_width: 0,
            geometry_root: 1,
            root_child: Some(window),
        },
        root: super::RootWindowEvidenceInput {
            active_window: None,
            raw_focused_window: None,
            focused_window: None,
            target_contains_focus: false,
            focus_ancestry_status: super::FocusAncestryStatus::NoFocus,
            current_workspace: Some(0),
        },
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
