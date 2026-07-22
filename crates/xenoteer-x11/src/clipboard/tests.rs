#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeMode {
    Healthy,
    EndlessEvents,
    PoisonPoll,
    PanicCommand,
    OwnershipRace,
}

struct CommandGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl CommandGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.entered.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(self.entered.load(Ordering::Acquire));
    }

    fn release(&self) {
        *lock_mutex(&self.released) = true;
        self.changed.notify_all();
    }

    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let guard = lock_mutex(&self.released);
        let (_guard, result) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(2), |released| !*released)
            .unwrap_or_else(|error| error.into_inner());
        assert!(!result.timed_out());
    }
}

struct FakeBackend {
    mode: FakeMode,
    gate: Option<Arc<CommandGate>>,
    poll_count: Arc<AtomicUsize>,
    endless_events: Arc<AtomicBool>,
    revisions: [u64; 2],
    values: [Option<ClipboardPayload>; 2],
    shutdown_called: Arc<AtomicBool>,
}

impl FakeBackend {
    fn new(mode: FakeMode) -> (Self, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let poll_count = Arc::new(AtomicUsize::new(0));
        let shutdown_called = Arc::new(AtomicBool::new(false));
        (
            Self {
                mode,
                gate: None,
                poll_count: Arc::clone(&poll_count),
                endless_events: Arc::new(AtomicBool::new(false)),
                revisions: [0, 0],
                values: [None, None],
                shutdown_called: Arc::clone(&shutdown_called),
            },
            poll_count,
            shutdown_called,
        )
    }

    fn index(selection: SelectionName) -> usize {
        match selection {
            SelectionName::Clipboard => 0,
            SelectionName::Primary => 1,
        }
    }

    fn evidence(
        &self,
        selection: SelectionName,
        payload: ClipboardPayload,
    ) -> RawClipboardReadResult {
        let target = match payload.kind() {
            ClipboardPayloadKind::Utf8Text => RawClipboardTarget::Utf8String,
            ClipboardPayloadKind::Binary(target) => target,
        };
        RawClipboardReadResult {
            selection,
            revision: self.revisions[Self::index(selection)],
            evidence: RawSelectionTransferEvidence {
                target,
                transfer: SelectionTransferMode::Direct,
                content_length: payload.byte_len() as u64,
                sha256: payload.digest(),
                owner_changed: false,
                terminal_chunk_observed: false,
                terminal: SelectionTransferTerminal::Completed,
            },
            payload,
        }
    }
}

impl ClipboardBackend for FakeBackend {
    fn poll_event(
        &mut self,
        _sender: &ClipboardEventSender,
    ) -> std::result::Result<bool, BackendFault> {
        if self.mode == FakeMode::PoisonPoll {
            return Err(BackendFault::Unavailable);
        }
        let has_event = self.endless_events.load(Ordering::Acquire);
        if has_event {
            self.poll_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(has_event)
    }

    fn handle_command(
        &mut self,
        command: ClipboardCommand,
        _sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        if let Some(gate) = &self.gate {
            gate.block();
            self.gate = None;
        }
        if self.mode == FakeMode::PanicCommand {
            panic!("contained fake backend panic");
        }
        if self.mode == FakeMode::OwnershipRace {
            command.fail(ClipboardActorFailureKind::OwnershipRace);
            return Ok(());
        }
        match command {
            ClipboardCommand::Set { request, reply } => {
                let index = Self::index(request.selection);
                self.revisions[index] = self.revisions[index].wrapping_add(1);
                self.values[index] = Some(request.payload);
                let _ignored = reply.send(Ok(ClipboardOwnershipEvidence {
                    selection: request.selection,
                    revision: self.revisions[index],
                    owner: 77,
                    server_time: 12,
                    verified: true,
                }));
            }
            ClipboardCommand::Clear { selection, reply } => {
                let index = Self::index(selection);
                self.revisions[index] = self.revisions[index].wrapping_add(1);
                self.values[index] = None;
                let _ignored = reply.send(Ok(ClipboardOwnershipEvidence {
                    selection,
                    revision: self.revisions[index],
                    owner: 0,
                    server_time: 13,
                    verified: true,
                }));
            }
            ClipboardCommand::Read { request, reply } => {
                let index = Self::index(request.selection);
                let result = self.values[index]
                    .clone()
                    .map(|payload| self.evidence(request.selection, payload));
                let _ignored = reply.send(result.ok_or_else(|| {
                    ClipboardActorFailure::new(ClipboardActorFailureKind::SelectionHasNoOwner)
                }));
            }
            ClipboardCommand::ObservePaste {
                request,
                ready,
                reply,
            } => {
                let _ignored = ready.send(Ok(()));
                let _ignored = reply.send(Ok(RawClipboardPasteObservation {
                    selection: request.selection,
                    request_observed: false,
                    requested_targets: Vec::new(),
                    transfer: None,
                }));
            }
        }
        self.endless_events.store(false, Ordering::Release);
        Ok(())
    }

    fn expire(
        &mut self,
        _now: Instant,
        _sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        Ok(())
    }

    fn counters(&self) -> (u8, u8) {
        (0, 0)
    }

    fn shutdown(&mut self, _kind: ClipboardActorFailureKind) {
        self.shutdown_called.store(true, Ordering::Release);
    }
}

fn spawn_fake(backend: FakeBackend, capacity: usize) -> (ClipboardActorHandle, ClipboardActorJoin) {
    let (handle, _events, join) = spawn_with_backend(capacity, 8, || Ok(backend)).unwrap();
    (handle, join)
}

fn set_request(selection: SelectionName, value: &str) -> ClipboardSetRequest {
    ClipboardSetRequest {
        selection,
        payload: ClipboardPayload::utf8_text(value).unwrap(),
        source: ClipboardOwnershipSource::Api,
    }
}

#[test]
fn bounded_event_loss_emits_one_barrier_before_later_events() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let event_loss = Arc::new(AtomicBool::new(false));
    let events = ClipboardEventSender {
        sender,
        event_loss: Arc::clone(&event_loss),
    };
    let receiver = ClipboardActorEventReceiver {
        receiver,
        event_loss,
    };
    let owner_event = |revision| ClipboardActorEvent::OwnershipChanged {
        selection: SelectionName::Clipboard,
        revision,
        owned: true,
    };

    events.emit(owner_event(1));
    events.emit(owner_event(2));
    events.emit(owner_event(3));

    assert_eq!(receiver.try_recv().unwrap(), owner_event(1));
    assert_eq!(
        receiver.try_recv().unwrap(),
        ClipboardActorEvent::ResyncRequired
    );
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

    events.emit(owner_event(4));
    assert_eq!(receiver.try_recv().unwrap(), owner_event(4));
}

#[test]
fn clipboard_and_primary_are_independent_and_join_initiates_shutdown() {
    let (backend, _polls, shutdown_called) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend, 8);
    let clipboard = handle
        .try_set(set_request(SelectionName::Clipboard, "clipboard-canary"))
        .unwrap();
    let primary = handle
        .try_set(set_request(SelectionName::Primary, "primary-canary"))
        .unwrap();
    assert_eq!(
        clipboard
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        primary
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let clear = handle.try_clear(SelectionName::Clipboard).unwrap();
    assert_eq!(
        clear
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    let read_primary = handle
        .try_read(ClipboardReadRawRequest {
            selection: SelectionName::Primary,
            preferred_targets: Vec::new(),
            allow_binary_fallback: false,
        })
        .unwrap();
    assert_eq!(
        read_primary
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .payload
            .expose_secret(),
        b"primary-canary"
    );
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
    assert!(shutdown_called.load(Ordering::Acquire));
}

#[test]
fn event_flood_is_fair_to_one_command_after_bounded_batch() {
    let (backend, polls, _shutdown) = FakeBackend::new(FakeMode::EndlessEvents);
    let endless_events = Arc::clone(&backend.endless_events);
    let (handle, join) = spawn_fake(backend, 2);
    endless_events.store(true, Ordering::Release);
    let reply = handle
        .try_set(set_request(SelectionName::Clipboard, "secret"))
        .unwrap();
    assert!(reply.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
    assert_eq!(polls.load(Ordering::Acquire), MAX_EVENTS_PER_TURN);
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
}

#[test]
fn saturated_ordinary_lane_cannot_block_coalesced_shutdown() {
    let gate = Arc::new(CommandGate::new());
    let (mut backend, _polls, shutdown_called) = FakeBackend::new(FakeMode::Healthy);
    backend.gate = Some(Arc::clone(&gate));
    let (handle, join) = spawn_fake(backend, 1);
    let first = handle
        .try_set(set_request(SelectionName::Clipboard, "one"))
        .unwrap();
    gate.wait_until_entered();
    let second = handle
        .try_set(set_request(SelectionName::Primary, "two"))
        .unwrap();
    assert_eq!(
        handle.try_clear(SelectionName::Clipboard).unwrap_err(),
        ClipboardSubmitError::QueueFull
    );
    let shutdown_one = handle.shutdown();
    let shutdown_two = handle.shutdown();
    gate.release();
    assert!(first.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
    assert_eq!(
        second
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .kind,
        ClipboardActorFailureKind::ActorStopped
    );
    assert!(
        shutdown_one
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert!(
        shutdown_two
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
    assert!(shutdown_called.load(Ordering::Acquire));
}

#[test]
fn backend_failure_poisoning_is_terminal_and_secret_free() {
    let (backend, _polls, shutdown_called) = FakeBackend::new(FakeMode::PoisonPoll);
    let (handle, join) = spawn_fake(backend, 2);
    let deadline = Instant::now() + Duration::from_secs(1);
    while handle.health().state == ClipboardActorState::Healthy && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(handle.health().state, ClipboardActorState::Poisoned);
    assert_eq!(
        handle.health().last_failure,
        Some(ClipboardActorFailureKind::BackendUnavailable)
    );
    assert_eq!(join.join(), ClipboardActorExit::Poisoned);
    assert!(shutdown_called.load(Ordering::Acquire));
}

#[test]
fn backend_panic_is_contained_at_actor_boundary() {
    let (backend, _polls, shutdown_called) = FakeBackend::new(FakeMode::PanicCommand);
    let (handle, join) = spawn_fake(backend, 2);
    let reply = handle
        .try_set(set_request(SelectionName::Clipboard, "panic-canary"))
        .unwrap();
    let _disconnected = reply.recv_timeout(Duration::from_secs(1));
    assert_eq!(join.join(), ClipboardActorExit::Panicked);
    assert!(!shutdown_called.load(Ordering::Acquire));
}

#[test]
fn ownership_races_are_typed_without_payloads() {
    let (backend, _polls, _shutdown) = FakeBackend::new(FakeMode::OwnershipRace);
    let (handle, join) = spawn_fake(backend, 2);
    let reply = handle
        .try_set(set_request(SelectionName::Clipboard, "race-canary"))
        .unwrap();
    let failure = reply
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.kind, ClipboardActorFailureKind::OwnershipRace);
    assert!(!format!("{failure:?}").contains("race-canary"));
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
}

#[test]
fn debug_surfaces_redact_secret_payload_hash_and_state_machine_bytes() {
    let canary = "do-not-log-clipboard-canary";
    let payload = ClipboardPayload::utf8_text(canary).unwrap();
    assert!(!format!("{payload:?}").contains(canary));
    assert!(!format!("{:?}", payload.digest()).contains(canary));

    let atoms = super::atoms::ClipboardAtoms::for_test();
    assert_eq!(
        atoms.identify_target(atoms.utf8_string),
        Some(RawClipboardTarget::Utf8String)
    );
    let state = super::receive::IncomingTransfer::new(
        SelectionName::Clipboard,
        1,
        atoms.private_properties[0],
        1,
        false,
        Instant::now(),
    );
    assert!(!format!("{state:?}").contains(canary));
}

#[test]
fn invalid_requests_are_rejected_before_admission() {
    let (backend, _polls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend, 2);
    assert_eq!(
        handle
            .try_observe_paste(ClipboardPasteObservationRequest {
                selection: SelectionName::Clipboard,
                timeout: Duration::ZERO,
            })
            .unwrap_err(),
        ClipboardSubmitError::InvalidRequest(ClipboardRequestError::InvalidTimeout)
    );
    assert_eq!(
        handle
            .try_read(ClipboardReadRawRequest {
                selection: SelectionName::Clipboard,
                preferred_targets: vec![RawClipboardTarget::Targets],
                allow_binary_fallback: false,
            })
            .unwrap_err(),
        ClipboardSubmitError::InvalidRequest(ClipboardRequestError::InvalidTarget)
    );
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
}

#[test]
fn paste_observation_ready_waits_until_backend_arm() {
    let gate = Arc::new(CommandGate::new());
    let (mut backend, _polls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    backend.gate = Some(Arc::clone(&gate));
    let (handle, join) = spawn_fake(backend, 2);
    let blocked = handle
        .try_set(set_request(SelectionName::Primary, "queue-blocker"))
        .unwrap();
    gate.wait_until_entered();
    let observation = handle
        .try_observe_paste(ClipboardPasteObservationRequest {
            selection: SelectionName::Clipboard,
            timeout: Duration::from_millis(100),
        })
        .unwrap();

    assert_eq!(
        observation.wait_until_ready(Duration::from_millis(20)),
        Err(RecvTimeoutError::Timeout)
    );
    gate.release();
    assert!(
        blocked
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert!(
        observation
            .wait_until_ready(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert!(
        observation
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert_eq!(join.join(), ClipboardActorExit::Stopped);
}

#[test]
fn latin1_string_is_advertised_only_when_lossless() {
    let latin = ClipboardPayload::utf8_text("café").unwrap();
    let non_latin = ClipboardPayload::utf8_text("snowman ☃").unwrap();
    assert!(
        latin
            .advertised_targets()
            .contains(&RawClipboardTarget::String)
    );
    assert_eq!(
        latin
            .representation(RawClipboardTarget::String)
            .unwrap()
            .as_ref(),
        b"caf\xe9"
    );
    assert!(
        !non_latin
            .advertised_targets()
            .contains(&RawClipboardTarget::String)
    );
    assert!(
        non_latin
            .representation(RawClipboardTarget::String)
            .is_none()
    );
}
