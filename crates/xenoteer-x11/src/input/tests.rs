use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use xenoteer_core::domain::RootPoint;
use xenoteer_core::input::{
    ButtonDirection, ButtonMapping, ClickAction, DragAction, InputAction, InputHealth,
    LogicalButton, MotionCurve, MotionOptions, MotionPolicy, MoveAction, PhysicalKey, PoisonReason,
    ScrollAction, ScrollDirection, WaypointDurationPolicy, plan_motion, plan_waypoint_motion,
};
use xenoteer_protocol::CommandId;

use crate::keyboard::{
    KeyIdentifier, KeyboardModelAvailability, KeyboardResolutionContext, NamedKey,
};

use super::actor::{
    InputActorExit, InputSubmitError, spawn_test_actor, spawn_test_actor_with_keyboard,
};
use super::backend::{
    BackendEvent, BackendFault, BackendFaultKind, BackendStartup, CoreKeyboardMapping,
    DrainedEvents, InputBackend, PointerObservation,
};
use super::execute::InputEngine;
use super::keyboard_model::{
    ActorKeyboardModel, CapturedKeyBinding, HeldBindingGeneration, KeyboardModelFault,
    KeyboardModelFaultKind, KeyboardReservation, ModelPreflight, RequiredModifierBinding,
};
use super::{
    ActionContext, ControlOutcome, InputCommand, InputFailureKind, InputOperation,
    InputOutcomeKind, KeyboardAction, KeyboardModelDiagnostics, KeyboardSequenceStep,
    PhysicalTextMode, PointerMoveRequest,
};

#[derive(Debug, Default)]
struct GateState {
    enabled: bool,
    entered: bool,
    released: bool,
}

#[derive(Debug, Default)]
struct SendGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl SendGate {
    fn enable(&self) {
        lock(&self.state).enabled = true;
    }

    fn wait_until_entered(&self) -> Result<(), &'static str> {
        let state = lock(&self.state);
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.entered)
            .map_err(|_| "send gate mutex poisoned")?;
        if timeout.timed_out() || !state.entered {
            Err("input actor did not enter the send gate")
        } else {
            Ok(())
        }
    }

    fn release(&self) {
        let mut state = lock(&self.state);
        state.released = true;
        self.changed.notify_all();
    }

    fn wait_if_enabled(&self) {
        let mut state = lock(&self.state);
        if !state.enabled || state.entered {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

#[derive(Debug)]
struct MockState {
    pointer: RootPoint,
    logical_buttons: [bool; 5],
    events: Vec<BackendEvent>,
    operations: Vec<MockOperation>,
    send_calls: usize,
    check_calls: usize,
    pointer_calls: usize,
    drain_calls: usize,
    send_failures: BTreeMap<usize, BackendFaultKind>,
    check_failures: BTreeMap<usize, BackendFaultKind>,
    pointer_failures: BTreeMap<usize, BackendFaultKind>,
    drains: BTreeMap<usize, DrainedEvents>,
    observations: VecDeque<PointerObservation>,
    cancel_on_pointer_call: Option<(usize, CancellationToken)>,
    cancel_on_drain_call: Option<(usize, CancellationToken)>,
    panic_on_send_call: Option<usize>,
    panic_after_send_call: Option<usize>,
    panic_on_pointer_call: Option<usize>,
    key_calls: usize,
    pressed_keys: Vec<PhysicalKey>,
    key_failures: BTreeMap<usize, BackendFaultKind>,
    keyboard_mappings: BTreeMap<u8, CoreKeyboardMapping>,
    keyboard_mapping_reads: usize,
    keyboard_mapping_writes: usize,
    keyboard_mapping_read_failures: BTreeMap<usize, BackendFaultKind>,
    keyboard_mapping_write_failures: BTreeMap<usize, BackendFaultKind>,
    keyboard_mapping_read_overrides: BTreeMap<usize, CoreKeyboardMapping>,
    delay_on_drain_call: Option<(usize, Duration)>,
    delay_on_pointer_call: Option<(usize, Duration)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MockOperation {
    Event(BackendEvent),
    MappingWrite(CoreKeyboardMapping),
}

#[derive(Clone, Debug)]
struct MockBackend {
    state: Arc<Mutex<MockState>>,
    gate: Arc<SendGate>,
}

#[derive(Debug)]
struct MockCookie {
    state: Arc<Mutex<MockState>>,
}

#[derive(Debug)]
struct MockKeyboardState {
    generation: u64,
    synchronize_calls: usize,
    synchronized_generations: VecDeque<u64>,
    synchronize_faults: BTreeMap<usize, KeyboardModelFaultKind>,
    resolve_faults: VecDeque<KeyboardModelFaultKind>,
    validate_calls: usize,
    validate_faults: BTreeMap<usize, KeyboardModelFaultKind>,
    held_faults: VecDeque<KeyboardModelFaultKind>,
    stale_held: bool,
    duplicate_shift_provider: bool,
    allow_reserved_scalar: bool,
    unknown_scalar_resolutions: usize,
    resolve_calls: usize,
    cancel_on_resolve: Option<(usize, CancellationToken)>,
    reservation_faults: VecDeque<KeyboardModelFaultKind>,
}

#[derive(Clone, Debug)]
struct MockKeyboardModel {
    state: Arc<Mutex<MockKeyboardState>>,
}

impl MockKeyboardModel {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockKeyboardState {
                generation: 1,
                synchronize_calls: 0,
                synchronized_generations: VecDeque::new(),
                synchronize_faults: BTreeMap::new(),
                resolve_faults: VecDeque::new(),
                validate_calls: 0,
                validate_faults: BTreeMap::new(),
                held_faults: VecDeque::new(),
                stale_held: false,
                duplicate_shift_provider: false,
                allow_reserved_scalar: false,
                unknown_scalar_resolutions: 0,
                resolve_calls: 0,
                cancel_on_resolve: None,
                reservation_faults: VecDeque::new(),
            })),
        }
    }

    fn boxed(&self) -> Box<dyn ActorKeyboardModel> {
        Box::new(self.clone())
    }

    fn set_stale_held(&self) {
        lock(&self.state).stale_held = true;
    }

    fn set_duplicate_shift_provider(&self) {
        lock(&self.state).duplicate_shift_provider = true;
    }

    fn fail_next_resolve(&self, kind: KeyboardModelFaultKind) {
        lock(&self.state).resolve_faults.push_back(kind);
    }

    fn fail_next_validate(&self, kind: KeyboardModelFaultKind) {
        let mut state = lock(&self.state);
        let call = state.validate_calls.saturating_add(1);
        state.validate_faults.insert(call, kind);
    }

    fn fail_validate_on_call(&self, call: usize, kind: KeyboardModelFaultKind) {
        lock(&self.state).validate_faults.insert(call, kind);
    }

    fn fail_next_held_validation(&self, kind: KeyboardModelFaultKind) {
        lock(&self.state).held_faults.push_back(kind);
    }

    fn enqueue_synchronized_generation(&self, generation: u64) {
        lock(&self.state)
            .synchronized_generations
            .push_back(generation);
    }

    fn allow_reserved_scalar(&self) {
        lock(&self.state).allow_reserved_scalar = true;
    }

    fn fail_synchronize_on_call(&self, call: usize, kind: KeyboardModelFaultKind) {
        lock(&self.state).synchronize_faults.insert(call, kind);
    }

    fn cancel_on_next_resolve(&self, cancellation: CancellationToken) {
        let mut state = lock(&self.state);
        let call = state.resolve_calls.saturating_add(1);
        state.cancel_on_resolve = Some((call, cancellation));
    }

    fn cancel_on_resolve_call(&self, call: usize, cancellation: CancellationToken) {
        lock(&self.state).cancel_on_resolve = Some((call, cancellation));
    }

    fn fail_next_reservation(&self, kind: KeyboardModelFaultKind) {
        lock(&self.state).reservation_faults.push_back(kind);
    }
}

impl ActorKeyboardModel for MockKeyboardModel {
    fn diagnostics(&self) -> KeyboardModelDiagnostics {
        let state = lock(&self.state);
        KeyboardModelDiagnostics {
            availability: KeyboardModelAvailability::Available,
            generation: Some(state.generation),
            keymap_fingerprint: Some(state.generation.wrapping_mul(17)),
        }
    }

    fn synchronize_preflight(&mut self) -> Result<ModelPreflight, KeyboardModelFault> {
        let mut state = lock(&self.state);
        state.synchronize_calls = state.synchronize_calls.saturating_add(1);
        let call = state.synchronize_calls;
        if let Some(kind) = state.synchronize_faults.remove(&call) {
            return Err(KeyboardModelFault::new(kind));
        }
        if let Some(generation) = state.synchronized_generations.pop_front() {
            state.generation = generation;
        }
        Ok(ModelPreflight {
            generation: state.generation,
        })
    }

    fn resolve_synchronized(
        &mut self,
        identifier: KeyIdentifier,
        _context: &KeyboardResolutionContext,
    ) -> Result<CapturedKeyBinding, KeyboardModelFault> {
        let mut state = lock(&self.state);
        state.resolve_calls = state.resolve_calls.saturating_add(1);
        let call = state.resolve_calls;
        if state
            .cancel_on_resolve
            .as_ref()
            .is_some_and(|(cancel_call, _)| *cancel_call == call)
            && let Some((_, cancellation)) = state.cancel_on_resolve.take()
        {
            cancellation.cancel();
        }
        if let Some(kind) = state.resolve_faults.pop_front() {
            return Err(KeyboardModelFault::new(kind));
        }
        let generation = state.generation;
        let duplicate_shift = state.duplicate_shift_provider;
        let (keycode, is_modifier, needs_shift) = match identifier {
            KeyIdentifier::Named(NamedKey::Enter) => (36, false, false),
            KeyIdentifier::Named(NamedKey::Escape) => (9, false, false),
            KeyIdentifier::Named(NamedKey::Shift | NamedKey::ShiftLeft) => (50, true, false),
            KeyIdentifier::Named(NamedKey::Control | NamedKey::ControlLeft) => (37, true, false),
            KeyIdentifier::Named(named) => {
                return Err(KeyboardModelFault::new(if named.is_modifier() {
                    KeyboardModelFaultKind::Unsafe
                } else {
                    KeyboardModelFaultKind::NotRepresentable
                }));
            }
            KeyIdentifier::Scalar('a') => (38, false, false),
            KeyIdentifier::Scalar('b') => (56, false, false),
            KeyIdentifier::Scalar('c') => (54, false, false),
            KeyIdentifier::Scalar('x') => (53, false, false),
            KeyIdentifier::Scalar('A' | '!') => (
                if identifier == KeyIdentifier::Scalar('A') {
                    38
                } else {
                    10
                },
                false,
                true,
            ),
            KeyIdentifier::Scalar(_) if state.allow_reserved_scalar => {
                state.unknown_scalar_resolutions =
                    state.unknown_scalar_resolutions.saturating_add(1);
                if state.unknown_scalar_resolutions == 1 {
                    return Err(KeyboardModelFault::new(
                        KeyboardModelFaultKind::NotRepresentable,
                    ));
                }
                (200, false, false)
            }
            KeyIdentifier::Scalar(_) => {
                return Err(KeyboardModelFault::new(
                    KeyboardModelFaultKind::NotRepresentable,
                ));
            }
            KeyIdentifier::Raw(keycode) => (keycode, false, false),
        };
        let shift = RequiredModifierBinding {
            key: physical_key(50)?,
            already_active: false,
        };
        let required_modifiers = if needs_shift {
            if duplicate_shift {
                vec![shift, shift]
            } else {
                vec![shift]
            }
        } else {
            Vec::new()
        };
        Ok(CapturedKeyBinding::for_test(
            identifier,
            physical_key(keycode)?,
            generation,
            is_modifier,
            required_modifiers,
            u64::from(keycode),
        ))
    }

    fn validate_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        _context: &KeyboardResolutionContext,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        let mut state = lock(&self.state);
        state.validate_calls = state.validate_calls.saturating_add(1);
        let call = state.validate_calls;
        if let Some(kind) = state.validate_faults.remove(&call) {
            return Err(KeyboardModelFault::new(kind));
        }
        if binding.generation != state.generation {
            return Err(KeyboardModelFault::new(
                KeyboardModelFaultKind::MappingChanged,
            ));
        }
        if binding.test_token() != Some(u64::from(binding.key.keycode())) {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        }
        Ok(ModelPreflight {
            generation: state.generation,
        })
    }

    fn validate_held_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        expected_identifier: KeyIdentifier,
        _context: &KeyboardResolutionContext,
    ) -> Result<HeldBindingGeneration, KeyboardModelFault> {
        let mut state = lock(&self.state);
        if let Some(kind) = state.held_faults.pop_front() {
            return Err(KeyboardModelFault::new(kind));
        }
        if binding.identifier != expected_identifier {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        }
        if state.stale_held || binding.generation != state.generation {
            Ok(HeldBindingGeneration::Stale {
                captured: binding.generation,
                current: state.generation.saturating_add(u64::from(state.stale_held)),
            })
        } else {
            Ok(HeldBindingGeneration::Current)
        }
    }

    fn reserve_unused_keycode(&mut self) -> Result<KeyboardReservation, KeyboardModelFault> {
        if let Some(kind) = lock(&self.state).reservation_faults.pop_front() {
            return Err(KeyboardModelFault::new(kind));
        }
        Ok(KeyboardReservation::for_test(physical_key(200)?, 1))
    }

    fn validate_reservation(
        &mut self,
        reservation: &KeyboardReservation,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        if reservation.test_token() != Some(1) {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        }
        Ok(ModelPreflight {
            generation: lock(&self.state).generation,
        })
    }
}

impl MockBackend {
    fn new(pointer: RootPoint) -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                pointer,
                logical_buttons: [false; 5],
                events: Vec::new(),
                operations: Vec::new(),
                send_calls: 0,
                check_calls: 0,
                pointer_calls: 0,
                drain_calls: 0,
                send_failures: BTreeMap::new(),
                check_failures: BTreeMap::new(),
                pointer_failures: BTreeMap::new(),
                drains: BTreeMap::new(),
                observations: VecDeque::new(),
                cancel_on_pointer_call: None,
                cancel_on_drain_call: None,
                panic_on_send_call: None,
                panic_after_send_call: None,
                panic_on_pointer_call: None,
                key_calls: 0,
                pressed_keys: Vec::new(),
                key_failures: BTreeMap::new(),
                keyboard_mappings: BTreeMap::from([(
                    200,
                    CoreKeyboardMapping {
                        keysyms_per_keycode: 4,
                        keysyms: vec![0; 4],
                    },
                )]),
                keyboard_mapping_reads: 0,
                keyboard_mapping_writes: 0,
                keyboard_mapping_read_failures: BTreeMap::new(),
                keyboard_mapping_write_failures: BTreeMap::new(),
                keyboard_mapping_read_overrides: BTreeMap::new(),
                delay_on_drain_call: None,
                delay_on_pointer_call: None,
            })),
            gate: Arc::new(SendGate::default()),
        }
    }

    fn fail_check(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state).check_failures.insert(call, kind);
    }

    fn fail_send(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state).send_failures.insert(call, kind);
    }

    fn fail_pointer(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state).pointer_failures.insert(call, kind);
    }

    fn mapping_change_on_drain(&self, call: usize) -> Result<(), BackendFault> {
        self.mapping_on_drain(call, &[1, 2, 3, 4, 5, 6, 7, 8, 9])
    }

    fn mapping_on_drain(&self, call: usize, server_map: &[u8]) -> Result<(), BackendFault> {
        lock(&self.state).drains.insert(
            call,
            DrainedEvents {
                pointer_mapping: Some(ButtonMapping::from_server(server_map).map_err(|error| {
                    BackendFault::new(BackendFaultKind::Capability, error.to_string())
                })?),
                ..DrainedEvents::default()
            },
        );
        Ok(())
    }

    fn cancel_on_pointer(&self, call: usize, token: CancellationToken) {
        lock(&self.state).cancel_on_pointer_call = Some((call, token));
    }

    fn panic_on_send(&self, call: usize) {
        lock(&self.state).panic_on_send_call = Some(call);
    }

    fn panic_after_send(&self, call: usize) {
        lock(&self.state).panic_after_send_call = Some(call);
    }

    fn cancel_on_drain(&self, call: usize, token: CancellationToken) {
        lock(&self.state).cancel_on_drain_call = Some((call, token));
    }

    fn panic_on_pointer(&self, call: usize) {
        lock(&self.state).panic_on_pointer_call = Some(call);
    }

    fn fail_key_observation(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state).key_failures.insert(call, kind);
    }

    fn delay_on_drain(&self, call: usize, duration: Duration) {
        lock(&self.state).delay_on_drain_call = Some((call, duration));
    }

    fn delay_on_pointer(&self, call: usize, duration: Duration) {
        lock(&self.state).delay_on_pointer_call = Some((call, duration));
    }

    fn push_observation(&self, pointer: RootPoint, logical_buttons: [bool; 5]) {
        lock(&self.state)
            .observations
            .push_back(PointerObservation {
                pointer,
                logical_buttons_1_to_5: logical_buttons,
            });
    }

    fn counts(&self) -> (usize, usize, usize) {
        let state = lock(&self.state);
        (state.events.len(), state.check_calls, state.pointer_calls)
    }

    fn events(&self) -> Vec<BackendEvent> {
        lock(&self.state).events.clone()
    }

    fn operations(&self) -> Vec<MockOperation> {
        lock(&self.state).operations.clone()
    }

    fn fail_keyboard_mapping_read(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state)
            .keyboard_mapping_read_failures
            .insert(call, kind);
    }

    fn fail_keyboard_mapping_write(&self, call: usize, kind: BackendFaultKind) {
        lock(&self.state)
            .keyboard_mapping_write_failures
            .insert(call, kind);
    }

    fn keyboard_mapping(&self, keycode: u8) -> Option<CoreKeyboardMapping> {
        lock(&self.state).keyboard_mappings.get(&keycode).cloned()
    }

    fn set_keyboard_mapping(&self, keycode: u8, mapping: CoreKeyboardMapping) {
        lock(&self.state).keyboard_mappings.insert(keycode, mapping);
    }

    fn override_keyboard_mapping_read(&self, call: usize, mapping: CoreKeyboardMapping) {
        lock(&self.state)
            .keyboard_mapping_read_overrides
            .insert(call, mapping);
    }
}

impl InputBackend for MockBackend {
    type Cookie<'a> = MockCookie;

    fn startup(&self) -> Result<BackendStartup, BackendFault> {
        Ok(BackendStartup {
            button_mapping: identity_mapping()?,
            min_keycode: 8,
            max_keycode: u8::MAX,
        })
    }

    fn drain_events(&self) -> Result<DrainedEvents, BackendFault> {
        let mut state = lock(&self.state);
        state.drain_calls = state.drain_calls.saturating_add(1);
        let call = state.drain_calls;
        if let Some((cancel_call, token)) = &state.cancel_on_drain_call
            && *cancel_call == call
        {
            token.cancel();
        }
        if let Some((delay_call, duration)) = state.delay_on_drain_call
            && delay_call == call
        {
            std::thread::sleep(duration);
        }
        Ok(state.drains.remove(&call).unwrap_or_default())
    }

    fn send_event(&self, event: BackendEvent) -> Result<Self::Cookie<'_>, BackendFault> {
        self.gate.wait_if_enabled();
        let mut state = lock(&self.state);
        state.send_calls = state.send_calls.saturating_add(1);
        let call = state.send_calls;
        if state.panic_on_send_call == Some(call) {
            drop(state);
            std::panic::resume_unwind(Box::new("injected input backend panic"));
        }
        if let Some(kind) = state.send_failures.remove(&call) {
            return Err(BackendFault::new(kind, "injected send failure"));
        }
        apply_mock_event(&mut state, event);
        state.events.push(event);
        state.operations.push(MockOperation::Event(event));
        if state.panic_after_send_call == Some(call) {
            drop(state);
            std::panic::resume_unwind(Box::new("injected post-serialization backend panic"));
        }
        Ok(MockCookie {
            state: Arc::clone(&self.state),
        })
    }

    fn check_cookie(cookie: Self::Cookie<'_>) -> Result<(), BackendFault> {
        let mut state = lock(&cookie.state);
        state.check_calls = state.check_calls.saturating_add(1);
        let call = state.check_calls;
        state.check_failures.remove(&call).map_or(Ok(()), |kind| {
            Err(BackendFault::new(kind, "injected check failure"))
        })
    }

    fn observe_pointer(&self) -> Result<PointerObservation, BackendFault> {
        let mut state = lock(&self.state);
        state.pointer_calls = state.pointer_calls.saturating_add(1);
        let call = state.pointer_calls;
        if state.panic_on_pointer_call == Some(call) {
            drop(state);
            std::panic::resume_unwind(Box::new("injected pointer panic"));
        }
        if let Some((cancel_call, token)) = &state.cancel_on_pointer_call
            && *cancel_call == call
        {
            token.cancel();
        }
        if let Some((delay_call, duration)) = state.delay_on_pointer_call
            && delay_call == call
        {
            std::thread::sleep(duration);
        }
        if let Some(kind) = state.pointer_failures.remove(&call) {
            return Err(BackendFault::new(kind, "injected pointer failure"));
        }
        if let Some(observation) = state.observations.pop_front() {
            return Ok(observation);
        }
        Ok(PointerObservation {
            pointer: state.pointer,
            logical_buttons_1_to_5: state.logical_buttons,
        })
    }

    fn observe_keys(&self) -> Result<Vec<xenoteer_core::input::PhysicalKey>, BackendFault> {
        let mut state = lock(&self.state);
        state.key_calls = state.key_calls.saturating_add(1);
        let call = state.key_calls;
        let pressed = state.pressed_keys.clone();
        state
            .key_failures
            .remove(&call)
            .map_or(Ok(pressed), |kind| {
                Err(BackendFault::new(kind, "injected key observation failure"))
            })
    }

    fn read_keyboard_mapping(&self, key: PhysicalKey) -> Result<CoreKeyboardMapping, BackendFault> {
        let mut state = lock(&self.state);
        state.keyboard_mapping_reads = state.keyboard_mapping_reads.saturating_add(1);
        let call = state.keyboard_mapping_reads;
        if let Some(kind) = state.keyboard_mapping_read_failures.remove(&call) {
            return Err(BackendFault::new(
                kind,
                "injected keyboard mapping read failure",
            ));
        }
        if let Some(mapping) = state.keyboard_mapping_read_overrides.remove(&call) {
            return Ok(mapping);
        }
        state
            .keyboard_mappings
            .get(&key.keycode())
            .cloned()
            .ok_or_else(|| BackendFault::new(BackendFaultKind::Capability, "missing mock mapping"))
    }

    fn write_keyboard_mapping(
        &self,
        key: PhysicalKey,
        mapping: &CoreKeyboardMapping,
    ) -> Result<(), BackendFault> {
        let mut state = lock(&self.state);
        state.keyboard_mapping_writes = state.keyboard_mapping_writes.saturating_add(1);
        let call = state.keyboard_mapping_writes;
        if let Some(kind) = state.keyboard_mapping_write_failures.remove(&call) {
            return Err(BackendFault::new(
                kind,
                "injected keyboard mapping write failure",
            ));
        }
        state
            .keyboard_mappings
            .insert(key.keycode(), mapping.clone());
        state
            .operations
            .push(MockOperation::MappingWrite(mapping.clone()));
        Ok(())
    }
}

fn identity_mapping() -> Result<ButtonMapping, BackendFault> {
    ButtonMapping::from_server(&[1, 2, 3, 4, 5, 6, 7, 8, 9])
        .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
}

fn apply_mock_event(state: &mut MockState, event: BackendEvent) {
    match event {
        BackendEvent::Motion { point, .. } => state.pointer = point,
        BackendEvent::Button {
            button, pressed, ..
        } if button.detail() <= 5 => {
            state.logical_buttons[usize::from(button.detail() - 1)] = pressed;
        }
        BackendEvent::Key { key, pressed, .. } => {
            if pressed {
                if !state.pressed_keys.contains(&key) {
                    state.pressed_keys.push(key);
                }
            } else if let Some(index) = state.pressed_keys.iter().position(|value| *value == key) {
                state.pressed_keys.remove(index);
            }
        }
        BackendEvent::Button { .. } => {}
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn physical_key(keycode: u8) -> Result<PhysicalKey, KeyboardModelFault> {
    PhysicalKey::new(keycode).map_err(|_| KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe))
}

fn click(count: u8) -> Result<InputAction, Box<dyn std::error::Error>> {
    Ok(InputAction::Click(ClickAction::new(
        None,
        LogicalButton::Left,
        count,
        0,
        0,
        1,
        250,
    )?))
}

fn instant_move(
    start: RootPoint,
    end: RootPoint,
) -> Result<InputAction, Box<dyn std::error::Error>> {
    Ok(InputAction::Move(MoveAction::new(plan_motion(
        start,
        end,
        MotionOptions::instant(false),
    )?)))
}

fn context() -> ActionContext {
    ActionContext::new(CommandId::new(), None)
}

fn failed<T>(
    result: Result<T, super::InputFailure>,
) -> Result<super::InputFailure, Box<dyn std::error::Error>> {
    match result {
        Ok(_) => Err(std::io::Error::other("expected input failure").into()),
        Err(failure) => Ok(failure),
    }
}

fn waypoint_plan(
    start: RootPoint,
    middle: RootPoint,
    end: RootPoint,
) -> Result<xenoteer_core::input::MotionPlan, Box<dyn std::error::Error>> {
    Ok(plan_waypoint_motion(
        start,
        &[middle, end],
        MotionCurve::Linear,
        MotionPolicy::default(),
        false,
        WaypointDurationPolicy::PerSegment(vec![0, 0]),
    )?)
}

#[test]
fn checked_cookie_failure_exhausts_cookies_before_distinct_reset()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    backend.fail_check(1, BackendFaultKind::Request);
    let mut engine = InputEngine::new(backend.clone())?;
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::CheckedRequestFailed);
    assert_eq!(failure.events_emitted, 2);
    assert!(
        failure
            .cleanup
            .as_ref()
            .is_some_and(|report| report.succeeded())
    );
    let (events, checks, barriers) = backend.counts();
    assert_eq!((events, checks, barriers), (3, 3, 1));
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn state_mutation_failure_after_send_is_terminal_without_more_backend_work()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    let mut engine = InputEngine::new(backend.clone())?;
    engine.fail_next_state_mutation_for_test();
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert!(!failure.progress_known);
    assert!(failure.cleanup.is_none());
    assert_eq!(backend.counts(), (1, 1, 0));
    assert!(engine.actor_panicked());
    Ok(())
}

#[test]
fn barrier_failure_then_reset_failure_poison_actor() -> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    backend.fail_pointer(1, BackendFaultKind::Request);
    backend.fail_pointer(2, BackendFaultKind::Request);
    let mut engine = InputEngine::new(backend)?;
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::BarrierFailed);
    assert!(
        failure
            .cleanup
            .as_ref()
            .is_some_and(|report| !report.succeeded())
    );
    assert!(matches!(
        engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::ResetFailed)
    ));
    Ok(())
}

#[test]
fn failed_poisoned_reset_retry_preserves_original_poison_without_invariant_panic()
-> Result<(), Box<dyn std::error::Error>> {
    let point = RootPoint::new(10, 10)?;
    let button = xenoteer_core::input::PhysicalButton::new(1)?;

    let request_backend = MockBackend::new(point);
    let mut request_engine = InputEngine::new(request_backend.clone())?;
    let _down = request_engine.execute(
        context(),
        InputAction::Button {
            button,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    )?;
    request_engine.poison_connection_for_test()?;
    request_backend.fail_check(2, BackendFaultKind::Request);
    let request_failure = failed(request_engine.reset_owned_input())?;
    assert_eq!(request_failure.kind, InputFailureKind::ResetFailed);
    assert_eq!(
        request_failure
            .cleanup
            .as_deref()
            .and_then(|report| report.detailed())
            .map(|report| report.residual_owned_buttons.as_slice()),
        Some([button].as_slice())
    );
    assert!(!request_engine.actor_panicked());
    assert_eq!(
        request_engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::ConnectionLost)
    );

    let barrier_backend = MockBackend::new(point);
    let mut barrier_engine = InputEngine::new(barrier_backend.clone())?;
    let _down = barrier_engine.execute(
        context(),
        InputAction::Button {
            button,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    )?;
    barrier_engine.poison_connection_for_test()?;
    barrier_backend.fail_pointer(2, BackendFaultKind::Request);
    let barrier_failure = failed(barrier_engine.reset_owned_input())?;
    assert_eq!(barrier_failure.kind, InputFailureKind::ResetFailed);
    assert_eq!(
        barrier_failure
            .cleanup
            .as_deref()
            .and_then(|report| report.detailed())
            .map(|report| report.residual_owned_buttons.as_slice()),
        Some([button].as_slice())
    );
    assert!(!barrier_engine.actor_panicked());
    assert_eq!(
        barrier_engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::ConnectionLost)
    );
    Ok(())
}

#[test]
fn cleanup_attempted_counts_actual_send_calls_before_first_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let point = RootPoint::new(10, 10)?;
    let first = xenoteer_core::input::PhysicalButton::new(1)?;
    let second = xenoteer_core::input::PhysicalButton::new(2)?;
    let backend = MockBackend::new(point);
    let mut engine = InputEngine::new(backend.clone())?;
    for button in [first, second] {
        let _down = engine.execute(
            context(),
            InputAction::Button {
                button,
                direction: ButtonDirection::Down,
                allow_redundant: false,
            },
            &CancellationToken::new(),
        )?;
    }
    backend.fail_send(3, BackendFaultKind::Request);
    let failure = failed(engine.reset_owned_input())?;
    assert_eq!(failure.kind, InputFailureKind::ResetFailed);
    let report = failure
        .cleanup
        .as_deref()
        .ok_or_else(|| std::io::Error::other("reset failure omitted cleanup evidence"))?;
    let report = report
        .detailed()
        .ok_or_else(|| std::io::Error::other("pointer cleanup was unexpectedly redacted"))?;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.confirmed, 0);
    assert_eq!(report.residual_owned_buttons, vec![first, second]);
    assert_eq!(backend.counts().0, 2);
    Ok(())
}

#[test]
fn mapping_change_after_logical_effect_is_explicit_and_nonretryable()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    backend.mapping_change_on_drain(3)?;
    let mut engine = InputEngine::new(backend)?;
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::ButtonMappingChangedAfterEffect
    );
    assert_eq!(failure.events_emitted, 2);
    assert!(failure.button_observation_partial);
    assert!(failure.effects.is_some());
    Ok(())
}

#[test]
fn logical_button_resolves_against_immediately_drained_mapping()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    backend.mapping_on_drain(2, &[3, 2, 1, 4, 5, 6, 7, 8, 9])?;
    let mut engine = InputEngine::new(backend.clone())?;
    let outcome = engine.execute(context(), click(1)?, &CancellationToken::new())?;
    assert_eq!(outcome.kind, InputOutcomeKind::Completed);
    assert!(matches!(
        backend.events().first(),
        Some(BackendEvent::Button {
            button,
            pressed: true,
            ..
        }) if button.detail() == 3
    ));
    Ok(())
}

#[test]
fn missing_and_ambiguous_logical_buttons_are_backend_capability_failures()
-> Result<(), Box<dyn std::error::Error>> {
    for server_map in [[0, 2, 3, 4, 5, 6, 7, 8, 9], [1, 1, 3, 4, 5, 6, 7, 8, 9]] {
        let backend = MockBackend::new(RootPoint::new(10, 10)?);
        backend.mapping_on_drain(2, &server_map)?;
        let mut engine = InputEngine::new(backend.clone())?;
        let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
        assert_eq!(failure.kind, InputFailureKind::UnsupportedByBackend);
        assert_eq!(failure.events_emitted, 0);
        assert_eq!(backend.counts().0, 0);
        assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    }
    Ok(())
}

#[test]
fn buttons_above_five_report_partial_observation_and_unobservable_reset()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    let mut engine = InputEngine::new(backend)?;
    let click = InputAction::Click(ClickAction::new(
        None,
        LogicalButton::Back,
        1,
        0,
        0,
        1,
        250,
    )?);
    let click_outcome = engine.execute(context(), click, &CancellationToken::new())?;
    assert!(click_outcome.button_observation_partial);

    let physical = xenoteer_core::input::PhysicalButton::new(8)?;
    let down = InputAction::Button {
        button: physical,
        direction: ButtonDirection::Down,
        allow_redundant: false,
    };
    let raw_outcome = engine.execute(context(), down, &CancellationToken::new())?;
    assert!(raw_outcome.button_observation_partial);
    let report = engine.reset_owned_input()?;
    assert_eq!(report.unobservable_buttons, vec![physical]);
    assert!(report.succeeded);
    Ok(())
}

#[test]
fn cancellation_between_clicks_reports_confirmed_partial_progress()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    let token = CancellationToken::new();
    backend.cancel_on_pointer(1, token.clone());
    let mut engine = InputEngine::new(backend)?;
    let outcome = engine.execute(context(), click(2)?, &token)?;
    assert_eq!(outcome.kind, InputOutcomeKind::CancelledAfterEffect);
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(outcome.events_emitted, 2);
    Ok(())
}

#[test]
fn duplicate_and_unowned_raw_transitions_fail_before_send() -> Result<(), Box<dyn std::error::Error>>
{
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    let button = xenoteer_core::input::PhysicalButton::new(1)?;
    let mut engine = InputEngine::new(backend.clone())?;
    let up = InputAction::Button {
        button,
        direction: ButtonDirection::Up,
        allow_redundant: false,
    };
    let failure = failed(engine.execute(context(), up, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::StateRejected);
    assert_eq!(backend.counts().0, 0);

    let down = InputAction::Button {
        button,
        direction: ButtonDirection::Down,
        allow_redundant: false,
    };
    let _outcome = engine.execute(context(), down.clone(), &CancellationToken::new())?;
    let failure = failed(engine.execute(context(), down, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::StateRejected);
    assert_eq!(backend.counts().0, 1);
    let _report = engine.reset_owned_input()?;
    Ok(())
}

#[test]
fn zero_event_logical_rejection_preserves_preexisting_owned_button()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(10, 10)?);
    let button = xenoteer_core::input::PhysicalButton::new(1)?;
    let mut engine = InputEngine::new(backend.clone())?;
    let down = InputAction::Button {
        button,
        direction: ButtonDirection::Down,
        allow_redundant: false,
    };
    let _outcome = engine.execute(context(), down, &CancellationToken::new())?;
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::StateRejected);
    assert_eq!(failure.events_emitted, 0);
    assert!(failure.cleanup.is_none());
    assert_eq!(backend.counts().0, 1);

    let report = engine.reset_owned_input()?;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.confirmed, 1);
    assert_eq!(backend.counts().0, 2);
    Ok(())
}

#[test]
fn semantic_pointer_interference_preserves_unrelated_raw_hold()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(10, 10)?;
    let end = RootPoint::new(20, 20)?;
    let interfered = RootPoint::new(15, 15)?;
    let backend = MockBackend::new(start);
    backend.push_observation(start, [false, true, false, false, false]);
    backend.push_observation(start, [false, true, false, false, false]);
    backend.push_observation(interfered, [false, true, false, false, false]);
    let mut engine = InputEngine::new(backend.clone())?;
    let held = xenoteer_core::input::PhysicalButton::new(2)?;
    let _down = engine.execute(
        context(),
        InputAction::Button {
            button: held,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    )?;
    let click = InputAction::Click(ClickAction::new(
        Some(plan_motion(start, end, MotionOptions::instant(false))?),
        LogicalButton::Left,
        1,
        0,
        0,
        1,
        250,
    )?);
    let failure = failed(engine.execute(context(), click, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(failure.events_emitted, 1);
    assert!(failure.cleanup.is_none());
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    assert_eq!(backend.counts().0, 2);

    let report = engine.reset_owned_input()?;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.confirmed, 1);
    assert_eq!(backend.counts().0, 3);
    Ok(())
}

#[test]
fn stuck_button_masks_retain_ownership_until_compensating_reset()
-> Result<(), Box<dyn std::error::Error>> {
    let point = RootPoint::new(10, 10)?;

    let click_backend = MockBackend::new(point);
    click_backend.push_observation(point, [true, false, false, false, false]);
    let mut click_engine = InputEngine::new(click_backend.clone())?;
    let click_failure =
        failed(click_engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(click_failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(
        click_failure
            .cleanup
            .as_deref()
            .map(|report| report.attempted()),
        Some(1)
    );
    assert_eq!(click_backend.counts().0, 3);

    let scroll_backend = MockBackend::new(point);
    scroll_backend.push_observation(point, [false, false, false, true, false]);
    let mut scroll_engine = InputEngine::new(scroll_backend.clone())?;
    let scroll_failure = failed(scroll_engine.execute(
        context(),
        InputAction::Scroll(ScrollAction::new(ScrollDirection::Up, 1, 0)?),
        &CancellationToken::new(),
    ))?;
    assert_eq!(scroll_failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(
        scroll_failure
            .cleanup
            .as_deref()
            .map(|report| report.attempted()),
        Some(1)
    );
    assert_eq!(scroll_backend.counts().0, 3);

    let drag_backend = MockBackend::new(point);
    let drag_end = RootPoint::new(20, 20)?;
    drag_backend.push_observation(point, [false; 5]);
    drag_backend.push_observation(point, [true, false, false, false, false]);
    drag_backend.push_observation(drag_end, [true, false, false, false, false]);
    drag_backend.push_observation(drag_end, [true, false, false, false, false]);
    let mut drag_engine = InputEngine::new(drag_backend.clone())?;
    let drag = InputAction::Drag(DragAction::new(
        plan_motion(point, drag_end, MotionOptions::instant(false))?,
        LogicalButton::Left,
        0,
        0,
    )?);
    let drag_failure = failed(drag_engine.execute(context(), drag, &CancellationToken::new()))?;
    assert_eq!(drag_failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(
        drag_failure
            .cleanup
            .as_deref()
            .map(|report| report.attempted()),
        Some(1)
    );
    assert_eq!(drag_backend.counts().0, 5);

    let raw_backend = MockBackend::new(point);
    let mut raw_engine = InputEngine::new(raw_backend.clone())?;
    let raw_button = xenoteer_core::input::PhysicalButton::new(1)?;
    let _down = raw_engine.execute(
        context(),
        InputAction::Button {
            button: raw_button,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    )?;
    raw_backend.push_observation(point, [true, false, false, false, false]);
    let raw_failure = failed(raw_engine.execute(
        context(),
        InputAction::Button {
            button: raw_button,
            direction: ButtonDirection::Up,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    ))?;
    assert_eq!(raw_failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(
        raw_failure
            .cleanup
            .as_deref()
            .map(|report| report.attempted()),
        Some(1)
    );
    assert_eq!(raw_backend.counts().0, 3);
    Ok(())
}

#[test]
fn fifo_queue_does_not_interleave_actions() -> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let middle = RootPoint::new(10, 10)?;
    let end = RootPoint::new(20, 20)?;
    let backend = MockBackend::new(start);
    let (handle, join) = spawn_test_actor(4, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let first = handle.try_submit(
        context(),
        instant_move(start, middle)?,
        CancellationToken::new(),
    )?;
    let second = handle.try_submit(
        context(),
        instant_move(middle, end)?,
        CancellationToken::new(),
    )?;
    let _first = first.blocking_recv()??;
    let _second = second.blocking_recv()??;
    let events = backend.events();
    assert!(matches!(events.as_slice(), [
        BackendEvent::Motion { point: first, .. },
        BackendEvent::Motion { point: second, .. }
    ] if *first == middle && *second == end));
    let shutdown = handle.shutdown();
    assert!(matches!(
        shutdown.blocking_recv()??,
        ControlOutcome::Shutdown(_)
    ));
    assert_eq!(join.join(), InputActorExit::Stopped);
    Ok(())
}

#[test]
fn queued_pointer_move_intent_interpolates_from_execution_time_position()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let middle = RootPoint::new(100, 100)?;
    let end = RootPoint::new(200, 100)?;
    let backend = MockBackend::new(start);
    let (handle, join) = spawn_test_actor(4, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;

    let first = handle.try_submit_pointer_move(
        context(),
        PointerMoveRequest::new(middle, MotionOptions::instant(false)),
        CancellationToken::new(),
    )?;
    let second = handle.try_submit_pointer_move(
        context(),
        PointerMoveRequest::new(
            end,
            MotionOptions::new(
                MotionCurve::Smooth,
                Some(100),
                MotionPolicy::default(),
                false,
            )?,
        ),
        CancellationToken::new(),
    )?;

    let _first = first.blocking_recv()??;
    let _second = second.blocking_recv()??;
    let points = backend
        .events()
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::Motion { point, .. } => Some(point),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(points.first(), Some(&middle));
    assert_eq!(points.last(), Some(&end));
    assert!(
        points[1..points.len() - 1]
            .iter()
            .any(|point| *point != middle && *point != end),
        "the second queued move must emit interpolated samples"
    );

    let shutdown = handle.shutdown();
    assert!(matches!(
        shutdown.blocking_recv()??,
        ControlOutcome::Shutdown(_)
    ));
    assert_eq!(join.join(), InputActorExit::Stopped);
    Ok(())
}

#[test]
fn full_normal_queue_cannot_block_coalesced_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let first_end = RootPoint::new(1, 1)?;
    let second_end = RootPoint::new(2, 2)?;
    let backend = MockBackend::new(start);
    backend.gate.enable();
    let (handle, join) = spawn_test_actor(1, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let first = handle.try_submit(
        context(),
        instant_move(start, first_end)?,
        CancellationToken::new(),
    )?;
    backend.gate.wait_until_entered()?;
    let second = handle.try_submit(
        context(),
        instant_move(first_end, second_end)?,
        CancellationToken::new(),
    )?;
    assert!(matches!(
        handle.try_submit(
            context(),
            instant_move(second_end, start)?,
            CancellationToken::new()
        ),
        Err(InputSubmitError::QueueFull)
    ));
    let reset = handle.reset();
    let shutdown = handle.shutdown();
    backend.gate.release();
    let _first = first.blocking_recv()??;
    let second_failure = failed(second.blocking_recv()?)?;
    assert_eq!(second_failure.kind, InputFailureKind::ActorStopped);
    assert_eq!(second_failure.requested_pointer, Some(second_end));
    assert!(matches!(reset.blocking_recv()??, ControlOutcome::Reset(_)));
    assert!(matches!(
        shutdown.blocking_recv()??,
        ControlOutcome::Shutdown(_)
    ));
    assert_eq!(join.join(), InputActorExit::Stopped);
    Ok(())
}

#[test]
fn backend_panic_is_typed_and_join_reports_panicked() -> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let backend = MockBackend::new(start);
    backend.panic_on_send(1);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let receiver = handle.try_submit(
        context(),
        instant_move(start, RootPoint::new(1, 1)?)?,
        CancellationToken::new(),
    )?;
    let failure = failed(receiver.blocking_recv()?)?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert!(!failure.progress_known);
    assert_eq!(failure.requested_pointer, Some(RootPoint::new(1, 1)?));
    assert_eq!(join.join(), InputActorExit::Panicked);
    assert_eq!(handle.health().thread, super::ActorThreadState::Panicked);
    Ok(())
}

#[test]
fn cancellation_and_deadline_after_initial_drain_send_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let end = RootPoint::new(1, 1)?;

    let cancel_backend = MockBackend::new(start);
    let token = CancellationToken::new();
    cancel_backend.cancel_on_drain(1, token.clone());
    let mut cancel_engine = InputEngine::new(cancel_backend.clone())?;
    let failure = failed(cancel_engine.execute(context(), instant_move(start, end)?, &token))?;
    assert_eq!(failure.kind, InputFailureKind::CancelledBeforeEffect);
    assert_eq!(cancel_backend.counts().0, 0);

    let deadline_backend = MockBackend::new(start);
    deadline_backend.delay_on_drain(1, Duration::from_millis(10));
    let mut deadline_engine = InputEngine::new(deadline_backend.clone())?;
    let deadline_context = ActionContext::new(
        CommandId::new(),
        Some(Instant::now() + Duration::from_millis(1)),
    );
    let failure = failed(deadline_engine.execute(
        deadline_context,
        instant_move(start, end)?,
        &CancellationToken::new(),
    ))?;
    assert_eq!(failure.kind, InputFailureKind::DeadlineExceededBeforeEffect);
    assert_eq!(failure.requested_pointer, Some(end));
    assert_eq!(deadline_backend.counts().0, 0);
    Ok(())
}

#[test]
fn deadline_after_final_move_barrier_counts_completed_primitive()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let end = RootPoint::new(1, 1)?;
    let backend = MockBackend::new(start);
    backend.delay_on_pointer(2, Duration::from_millis(10));
    let mut engine = InputEngine::new(backend)?;
    let deadline_context = ActionContext::new(
        CommandId::new(),
        Some(Instant::now() + Duration::from_millis(1)),
    );
    let outcome = engine.execute(
        deadline_context,
        instant_move(start, end)?,
        &CancellationToken::new(),
    )?;
    assert_eq!(outcome.kind, InputOutcomeKind::DeadlineExceededAfterEffect);
    assert_eq!(outcome.events_emitted, 1);
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(outcome.observed_pointer, Some(end));
    Ok(())
}

#[test]
fn move_observes_endpoint_interference_without_failing_or_poisoning()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let end = RootPoint::new(2, 2)?;
    let interfered = RootPoint::new(1, 1)?;
    let backend = MockBackend::new(start);
    backend.push_observation(start, [false; 5]);
    backend.push_observation(interfered, [false; 5]);
    let mut engine = InputEngine::new(backend)?;
    let outcome = engine.execute(
        context(),
        instant_move(start, end)?,
        &CancellationToken::new(),
    )?;
    assert_eq!(outcome.kind, InputOutcomeKind::Completed);
    assert_eq!(outcome.requested_pointer, Some(end));
    assert_eq!(outcome.observed_pointer, Some(interfered));
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn deadline_after_final_drag_segment_still_releases_and_counts_primitive()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let end = RootPoint::new(1, 1)?;
    let backend = MockBackend::new(start);
    backend.delay_on_pointer(3, Duration::from_millis(10));
    let mut engine = InputEngine::new(backend.clone())?;
    let deadline_context = ActionContext::new(
        CommandId::new(),
        Some(Instant::now() + Duration::from_millis(1)),
    );
    let action = InputAction::Drag(DragAction::new(
        plan_motion(start, end, MotionOptions::instant(false))?,
        LogicalButton::Left,
        0,
        0,
    )?);
    let outcome = engine.execute(deadline_context, action, &CancellationToken::new())?;
    assert_eq!(outcome.kind, InputOutcomeKind::DeadlineExceededAfterEffect);
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(outcome.events_emitted, 4);
    assert_eq!(outcome.observed_pointer, Some(end));
    assert!(matches!(
        backend.events().last(),
        Some(BackendEvent::Button { pressed: false, .. })
    ));
    Ok(())
}

#[test]
fn later_batch_failure_retains_prior_pointer_observation() -> Result<(), Box<dyn std::error::Error>>
{
    let start = RootPoint::new(0, 0)?;
    let end = RootPoint::new(2, 2)?;
    let backend = MockBackend::new(start);
    backend.fail_check(4, BackendFaultKind::Request);
    let movement = plan_motion(start, end, MotionOptions::instant(false))?;
    let action = InputAction::Click(ClickAction::new(
        Some(movement),
        LogicalButton::Left,
        2,
        0,
        0,
        1,
        250,
    )?);
    let mut engine = InputEngine::new(backend)?;
    let failure = failed(engine.execute(context(), action, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::CheckedRequestFailed);
    assert_eq!(failure.last_observed_pointer, Some(end));
    assert_eq!(failure.completed_units, 1);
    Ok(())
}

#[test]
fn connection_cookie_failure_dominates_request_error_and_suppresses_barrier()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_check(1, BackendFaultKind::Request);
    backend.fail_check(2, BackendFaultKind::Connection);
    let mut engine = InputEngine::new(backend.clone())?;
    let failure = failed(engine.execute(context(), click(1)?, &CancellationToken::new()))?;
    assert_eq!(failure.kind, InputFailureKind::BackendUnavailable);
    assert_eq!(failure.events_emitted, 2);
    let (_events, checks, barriers) = backend.counts();
    assert_eq!(checks, 3);
    assert_eq!(barriers, 1, "only best-effort reset may issue a barrier");
    assert!(matches!(
        engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::ConnectionLost)
    ));
    Ok(())
}

#[test]
fn waypoint_move_stops_at_confirmed_segment_boundary() -> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let middle = RootPoint::new(5, 5)?;
    let end = RootPoint::new(10, 10)?;
    let backend = MockBackend::new(start);
    let token = CancellationToken::new();
    backend.cancel_on_pointer(2, token.clone());
    let mut engine = InputEngine::new(backend.clone())?;
    let action = InputAction::Move(MoveAction::new(waypoint_plan(start, middle, end)?));
    let outcome = engine.execute(context(), action, &token)?;
    assert_eq!(outcome.kind, InputOutcomeKind::CancelledAfterEffect);
    assert_eq!(outcome.events_emitted, 1);
    assert_eq!(outcome.completed_units, 0);
    assert_eq!(outcome.observed_pointer, Some(middle));
    assert_eq!(backend.events().len(), 1);
    Ok(())
}

#[test]
fn waypoint_drag_cancellation_releases_at_segment_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    let start = RootPoint::new(0, 0)?;
    let middle = RootPoint::new(5, 5)?;
    let end = RootPoint::new(10, 10)?;
    let backend = MockBackend::new(start);
    let token = CancellationToken::new();
    backend.cancel_on_pointer(3, token.clone());
    let mut engine = InputEngine::new(backend.clone())?;
    let action = InputAction::Drag(DragAction::new(
        waypoint_plan(start, middle, end)?,
        LogicalButton::Left,
        40,
        60,
    )?);
    let outcome = engine.execute(context(), action, &token)?;
    assert_eq!(outcome.kind, InputOutcomeKind::CancelledAfterEffect);
    assert_eq!(outcome.completed_units, 0);
    assert_eq!(outcome.events_emitted, 4);
    assert_eq!(outcome.observed_pointer, Some(middle));
    assert!(matches!(
        backend.events().last(),
        Some(BackendEvent::Button {
            pressed: false,
            delay_ms: 60,
            ..
        })
    ));
    assert_eq!(outcome.observed_logical_buttons_1_to_5, Some([false; 5]));
    Ok(())
}

#[test]
fn compound_actions_emit_exact_server_delay_order() -> Result<(), Box<dyn std::error::Error>> {
    let point = RootPoint::new(0, 0)?;
    let click_backend = MockBackend::new(point);
    let mut click_engine = InputEngine::new(click_backend.clone())?;
    let click_action = InputAction::Click(ClickAction::new(
        None,
        LogicalButton::Left,
        2,
        30,
        50,
        100,
        250,
    )?);
    let _outcome = click_engine.execute(context(), click_action, &CancellationToken::new())?;
    let click_delays: Vec<_> = click_backend.events().iter().map(event_delay).collect();
    assert_eq!(click_delays, vec![30, 50, 100, 50]);

    let drag_end = RootPoint::new(2, 2)?;
    let drag_backend = MockBackend::new(point);
    let mut drag_engine = InputEngine::new(drag_backend.clone())?;
    let drag_action = InputAction::Drag(DragAction::new(
        match instant_move(point, drag_end)? {
            InputAction::Move(action) => action.plan().clone(),
            _ => return Err(std::io::Error::other("move helper returned wrong action").into()),
        },
        LogicalButton::Left,
        40,
        60,
    )?);
    let _outcome = drag_engine.execute(context(), drag_action, &CancellationToken::new())?;
    let drag_delays: Vec<_> = drag_backend.events().iter().map(event_delay).collect();
    assert_eq!(drag_delays, vec![0, 40, 0, 60]);

    let scroll_backend = MockBackend::new(point);
    let mut scroll_engine = InputEngine::new(scroll_backend.clone())?;
    let scroll = InputAction::Scroll(ScrollAction::new(ScrollDirection::Down, 2, 16)?);
    let _outcome = scroll_engine.execute(context(), scroll, &CancellationToken::new())?;
    let scroll_delays: Vec<_> = scroll_backend.events().iter().map(event_delay).collect();
    assert_eq!(scroll_delays, vec![0, 0, 16, 0]);
    Ok(())
}

#[test]
fn cleanup_keeps_pointer_and_key_evidence_independent() -> Result<(), Box<dyn std::error::Error>> {
    let key = PhysicalKey::new(38)?;
    let key_failure_backend = MockBackend::new(RootPoint::new(0, 0)?);
    key_failure_backend.fail_key_observation(1, BackendFaultKind::Request);
    let mut key_failure_engine = InputEngine::new(key_failure_backend)?;
    key_failure_engine.seed_owned_key_for_test(key, false)?;
    let failure = failed(key_failure_engine.reset_owned_input())?;
    let report = failure
        .cleanup
        .ok_or_else(|| std::io::Error::other("missing cleanup report"))?;
    assert!(matches!(
        report.as_ref(),
        super::InputCleanupEvidence::RedactedKeyboard {
            pointer_observation_available: true,
            key_observation_available: false,
            residual_owned_key_count: 1,
            ..
        }
    ));

    let pointer_failure_backend = MockBackend::new(RootPoint::new(0, 0)?);
    pointer_failure_backend.fail_pointer(1, BackendFaultKind::Request);
    let mut pointer_failure_engine = InputEngine::new(pointer_failure_backend)?;
    pointer_failure_engine.seed_owned_key_for_test(key, false)?;
    let failure = failed(pointer_failure_engine.reset_owned_input())?;
    let report = failure
        .cleanup
        .ok_or_else(|| std::io::Error::other("missing cleanup report"))?;
    assert!(matches!(
        report.as_ref(),
        super::InputCleanupEvidence::RedactedKeyboard {
            pointer_observation_available: false,
            key_observation_available: true,
            residual_owned_key_count: 1,
            ..
        }
    ));
    Ok(())
}

#[test]
fn control_backend_panic_is_typed_and_join_panics() -> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.panic_on_pointer(1);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let failure = failed(handle.probe().blocking_recv()?)?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert!(!failure.progress_known);
    assert_eq!(join.join(), InputActorExit::Panicked);
    assert_eq!(handle.health().thread, super::ActorThreadState::Panicked);
    Ok(())
}

fn execute_keyboard_action(
    engine: &mut InputEngine<MockBackend>,
    action: KeyboardAction,
) -> Result<super::InputOutcome, super::InputFailure> {
    engine.execute_operation(
        context(),
        InputOperation::Keyboard(action),
        &CancellationToken::new(),
    )
}

fn key_events(events: &[BackendEvent]) -> Vec<(u8, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::Key { key, pressed, .. } => Some((key.keycode(), *pressed)),
            BackendEvent::Motion { .. } | BackendEvent::Button { .. } => None,
        })
        .collect()
}

#[test]
fn keyboard_chord_is_modifier_first_and_releases_in_reverse()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let outcome = execute_keyboard_action(
        &mut engine,
        KeyboardAction::chord(
            &[
                KeyIdentifier::Named(NamedKey::ControlLeft),
                KeyIdentifier::Scalar('a'),
            ],
            0,
        )?,
    )?;
    assert_eq!(outcome.kind, InputOutcomeKind::Completed);
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(
        key_events(&backend.events()),
        vec![(37, true), (38, true), (38, false), (37, false)]
    );
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn keyboard_deduplicates_one_modifier_provider_in_multiple_groups()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    keyboard.set_duplicate_shift_provider();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Scalar('A'), 0)?,
    )?;
    assert_eq!(
        key_events(&backend.events()),
        vec![(50, true), (38, true), (38, false), (50, false)]
    );
    Ok(())
}

#[test]
fn held_keys_reference_count_shared_synthesized_modifier() -> Result<(), Box<dyn std::error::Error>>
{
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    for action in [
        KeyboardAction::down(KeyIdentifier::Scalar('A'))?,
        KeyboardAction::down(KeyIdentifier::Scalar('!'))?,
        KeyboardAction::up(KeyIdentifier::Scalar('A'))?,
        KeyboardAction::up(KeyIdentifier::Scalar('!'))?,
    ] {
        execute_keyboard_action(&mut engine, action)?;
    }
    assert_eq!(
        key_events(&backend.events()),
        vec![
            (50, true),
            (38, true),
            (10, true),
            (38, false),
            (10, false),
            (50, false),
        ]
    );
    Ok(())
}

#[test]
fn stale_held_binding_releases_exact_capture_then_reports_after_effect()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    execute_keyboard_action(
        &mut engine,
        KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?,
    )?;
    keyboard.set_stale_held();
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::up(KeyIdentifier::Named(NamedKey::Enter))?,
    ))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::KeyboardMappingChangedAfterEffect
    );
    assert_eq!(failure.events_emitted, 1);
    assert_eq!(key_events(&backend.events()), vec![(36, true), (36, false)]);
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn held_validation_failure_cannot_leave_a_healthy_wedged_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    execute_keyboard_action(
        &mut engine,
        KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?,
    )?;
    keyboard.fail_next_held_validation(KeyboardModelFaultKind::MappingChanged);
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::up(KeyIdentifier::Named(NamedKey::Enter))?,
    ))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::KeyboardMappingChangedAfterEffect
    );
    assert_eq!(key_events(&backend.events()), vec![(36, true), (36, false)]);
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn post_effect_modifier_interference_is_nonretryable_and_redacted()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    keyboard.fail_validate_on_call(2, KeyboardModelFaultKind::Conflict);
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Scalar('a'), 0)?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::ModifierConflictAfterEffect);
    assert_eq!(failure.events_emitted, 2);
    assert_eq!(
        key_events(&backend.events()),
        vec![(38, true), (38, false), (38, false)]
    );
    assert!(
        failure
            .cleanup
            .as_ref()
            .is_some_and(|report| report.succeeded())
    );
    Ok(())
}

#[test]
fn extended_text_installs_one_scalar_and_restores_exact_mapping()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let original = backend
        .keyboard_mapping(200)
        .ok_or_else(|| std::io::Error::other("missing original mapping"))?;
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let outcome = execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    )?;
    assert_eq!(
        key_events(&backend.events()),
        vec![(200, true), (200, false)]
    );
    assert_eq!(backend.keyboard_mapping(200), Some(original));
    let evidence = outcome
        .keyboard
        .ok_or_else(|| std::io::Error::other("missing keyboard evidence"))?;
    assert_eq!(
        evidence.requested_text_mode,
        Some(PhysicalTextMode::ExtendedTemporaryMapping)
    );
    assert_eq!(evidence.current_layout_scalars, 0);
    assert_eq!(evidence.temporary_mapping_scalars, 1);
    assert_eq!(evidence.temporary_mappings_installed, 1);
    assert_eq!(evidence.temporary_mappings_restored, 1);
    assert_eq!(evidence.temporary_mapping_restoration_proven, Some(true));
    Ok(())
}

#[test]
fn temporary_mapping_accepts_only_xkb_canonical_slot_two_duplication()
-> Result<(), Box<dyn std::error::Error>> {
    const SCALAR_KEYSYM: u32 = 0x0101_f575;

    let accepted_backend = MockBackend::new(RootPoint::new(0, 0)?);
    accepted_backend.set_keyboard_mapping(
        200,
        CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![0; 7],
        },
    );
    accepted_backend.override_keyboard_mapping_read(
        2,
        CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![SCALAR_KEYSYM, 0, SCALAR_KEYSYM, 0, 0, 0, 0],
        },
    );
    let accepted_keyboard = MockKeyboardModel::new();
    accepted_keyboard.allow_reserved_scalar();
    let mut accepted_engine =
        InputEngine::new_with_keyboard(accepted_backend.clone(), accepted_keyboard.boxed())?;
    let accepted = execute_keyboard_action(
        &mut accepted_engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    )?;
    assert_eq!(accepted.kind, InputOutcomeKind::Completed);
    assert_eq!(
        key_events(&accepted_backend.events()),
        vec![(200, true), (200, false)]
    );
    assert_eq!(
        accepted_backend.keyboard_mapping(200),
        Some(CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![0; 7],
        })
    );

    for malformed in [
        CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![SCALAR_KEYSYM, SCALAR_KEYSYM, 0, 0, 0, 0, 0],
        },
        CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![SCALAR_KEYSYM, 0, SCALAR_KEYSYM, 0, SCALAR_KEYSYM, 0, 0],
        },
        CoreKeyboardMapping {
            keysyms_per_keycode: 1,
            keysyms: vec![SCALAR_KEYSYM],
        },
        CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![SCALAR_KEYSYM, 0, SCALAR_KEYSYM],
        },
    ] {
        let backend = MockBackend::new(RootPoint::new(0, 0)?);
        let original = CoreKeyboardMapping {
            keysyms_per_keycode: 7,
            keysyms: vec![0; 7],
        };
        backend.set_keyboard_mapping(200, original.clone());
        backend.override_keyboard_mapping_read(2, malformed);
        let keyboard = MockKeyboardModel::new();
        keyboard.allow_reserved_scalar();
        let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
        let failure = failed(execute_keyboard_action(
            &mut engine,
            KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
        ))?;
        assert_eq!(
            failure.kind,
            InputFailureKind::TemporaryMappingInstallFailed
        );
        assert!(backend.events().is_empty());
        assert_eq!(backend.keyboard_mapping(200), Some(original));
    }
    Ok(())
}

#[test]
fn cancellation_after_temporary_install_restores_without_xtest()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let original = backend
        .keyboard_mapping(200)
        .ok_or_else(|| std::io::Error::other("missing original mapping"))?;
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let cancellation = CancellationToken::new();
    keyboard.cancel_on_resolve_call(2, cancellation.clone());
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(engine.execute_operation(
        context(),
        InputOperation::Keyboard(KeyboardAction::physical_text(
            "🕵",
            PhysicalTextMode::ExtendedTemporaryMapping,
            0,
        )?),
        &cancellation,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::CancelledBeforeEffect);
    assert_eq!(failure.events_emitted, 0);
    assert!(backend.events().is_empty());
    assert_eq!(backend.keyboard_mapping(200), Some(original));
    assert!(failure.cleanup.as_ref().is_none());
    let operations = backend.operations();
    assert!(matches!(
        operations.as_slice(),
        [
            MockOperation::MappingWrite(installed),
            MockOperation::MappingWrite(restored)
        ] if installed.keysyms.first().is_some_and(|keysym| *keysym != 0)
            && restored.keysyms.iter().all(|keysym| *keysym == 0)
    ));
    Ok(())
}

#[test]
fn temporary_mapping_install_failure_restores_before_returning()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let original = backend
        .keyboard_mapping(200)
        .ok_or_else(|| std::io::Error::other("missing original mapping"))?;
    backend.fail_keyboard_mapping_write(1, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::TemporaryMappingInstallFailed
    );
    assert_eq!(failure.events_emitted, 0);
    assert_eq!(backend.keyboard_mapping(200), Some(original));
    let evidence = failure
        .keyboard
        .ok_or_else(|| std::io::Error::other("missing keyboard failure evidence"))?;
    assert_eq!(evidence.temporary_mappings_installed, 0);
    assert_eq!(evidence.temporary_mappings_restored, 1);
    assert_eq!(evidence.temporary_mapping_restoration_proven, Some(true));
    assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    Ok(())
}

#[test]
fn temporary_mapping_snapshot_read_failure_is_typed_before_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_keyboard_mapping_read(1, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::CheckedRequestFailed);
    assert!(backend.operations().is_empty());
    Ok(())
}

#[test]
fn temporary_restore_failure_is_poisoned_and_automatic_reset_retries()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let original = backend
        .keyboard_mapping(200)
        .ok_or_else(|| std::io::Error::other("missing original mapping"))?;
    backend.fail_keyboard_mapping_write(2, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::TemporaryMappingRestoreFailed
    );
    assert_eq!(backend.keyboard_mapping(200), Some(original));
    assert!(failure.cleanup.as_ref().is_some_and(|report| {
        matches!(
            report.as_ref(),
            super::InputCleanupEvidence::RedactedKeyboard {
                temporary_mapping_restore_attempted: true,
                temporary_mapping_restore_proven: true,
                ..
            }
        )
    }));
    assert_eq!(
        engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::TemporaryKeyboardMappingRestoreFailed)
    );
    Ok(())
}

#[test]
fn missing_named_key_is_unsupported_but_missing_scalar_is_text_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    keyboard.fail_next_resolve(KeyboardModelFaultKind::NotRepresentable);
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let named = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::F24), 0)?,
    ))?;
    assert_eq!(named.kind, InputFailureKind::UnsupportedByBackend);

    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let scalar = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::CurrentLayout, 0)?,
    ))?;
    assert_eq!(scalar.kind, InputFailureKind::TextNotRepresentable);
    Ok(())
}

#[test]
fn keyboard_debug_output_never_contains_scalar_content() -> Result<(), Box<dyn std::error::Error>> {
    let secret = '🕵';
    let step = KeyboardSequenceStep::press(KeyIdentifier::Scalar(secret), 1, 2)?;
    let action = KeyboardAction::physical_text(
        secret.to_string(),
        PhysicalTextMode::ExtendedTemporaryMapping,
        0,
    )?;
    let operation = InputOperation::Keyboard(action.clone());
    let (reply, _receiver) = tokio::sync::oneshot::channel();
    let command = InputCommand {
        context: context(),
        operation: operation.clone(),
        cancellation: CancellationToken::new(),
        reply,
    };
    for rendered in [
        format!("{step:?}"),
        format!("{action:?}"),
        format!("{operation:?}"),
        format!("{command:?}"),
    ] {
        assert!(!rendered.contains(secret));
    }
    Ok(())
}

#[test]
fn scalar_actions_structurally_redact_public_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let down = execute_keyboard_action(
        &mut engine,
        KeyboardAction::down(KeyIdentifier::Scalar('a'))?,
    )?;
    assert!(matches!(
        down.effects,
        super::InputEffectEvidence::RedactedKeyboard {
            provisional: 0,
            confirmed: 1
        }
    ));
    assert!(down.keyboard.as_ref().is_some_and(|value| {
        value.bindings.is_empty() && value.model.keymap_fingerprint.is_none()
    }));
    let up = execute_keyboard_action(&mut engine, KeyboardAction::up(KeyIdentifier::Scalar('a'))?)?;
    assert!(matches!(
        up.effects,
        super::InputEffectEvidence::RedactedKeyboard { .. }
    ));

    let mixed = execute_keyboard_action(
        &mut engine,
        KeyboardAction::sequence(&[
            KeyboardSequenceStep::press(KeyIdentifier::Named(NamedKey::Enter), 0, 0)?,
            KeyboardSequenceStep::press(KeyIdentifier::Scalar('a'), 0, 0)?,
        ])?,
    )?;
    assert!(matches!(
        mixed.effects,
        super::InputEffectEvidence::RedactedKeyboard {
            provisional: 0,
            confirmed: 4
        }
    ));
    assert!(
        mixed
            .keyboard
            .as_ref()
            .is_some_and(|value| value.bindings.is_empty())
    );
    Ok(())
}

#[test]
fn scalar_partial_failure_redacts_effect_cleanup_and_model_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_check(1, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Scalar('a'), 0)?,
    ))?;
    assert!(matches!(
        failure.effects.as_deref(),
        Some(super::InputEffectEvidence::RedactedKeyboard { .. })
    ));
    assert!(matches!(
        failure.cleanup.as_deref(),
        Some(super::InputCleanupEvidence::RedactedKeyboard { .. })
    ));
    assert!(failure.keyboard.as_ref().is_some_and(|value| {
        value.bindings.is_empty() && value.model.keymap_fingerprint.is_none()
    }));
    assert!(!format!("{failure:?}").contains("PhysicalKey"));
    Ok(())
}

#[test]
fn named_keyboard_action_retains_detailed_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let outcome = execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
    )?;
    assert!(matches!(
        outcome.effects,
        super::InputEffectEvidence::Journal(_)
    ));
    assert!(
        outcome
            .keyboard
            .as_ref()
            .is_some_and(|value| value.bindings.len() == 1)
    );
    Ok(())
}

#[test]
fn cancellation_during_resolution_stops_before_first_xtest()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let cancellation = CancellationToken::new();
    keyboard.cancel_on_next_resolve(cancellation.clone());
    let failure = failed(engine.execute_operation(
        context(),
        InputOperation::Keyboard(KeyboardAction::press(
            KeyIdentifier::Named(NamedKey::Enter),
            0,
        )?),
        &cancellation,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::CancelledBeforeEffect);
    assert_eq!(failure.events_emitted, 0);
    assert!(backend.events().is_empty());
    Ok(())
}

#[test]
fn final_cancellation_is_ignored_but_between_units_stops() -> Result<(), Box<dyn std::error::Error>>
{
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let final_cancel = CancellationToken::new();
    backend.cancel_on_pointer(1, final_cancel.clone());
    let completed = engine.execute_operation(
        context(),
        InputOperation::Keyboard(KeyboardAction::press(
            KeyIdentifier::Named(NamedKey::Enter),
            0,
        )?),
        &final_cancel,
    )?;
    assert_eq!(completed.kind, InputOutcomeKind::Completed);

    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let between_cancel = CancellationToken::new();
    backend.cancel_on_pointer(1, between_cancel.clone());
    let outcome = engine.execute_operation(
        context(),
        InputOperation::Keyboard(KeyboardAction::sequence(&[
            KeyboardSequenceStep::press(KeyIdentifier::Named(NamedKey::Enter), 0, 0)?,
            KeyboardSequenceStep::press(KeyIdentifier::Named(NamedKey::Escape), 0, 0)?,
        ])?),
        &between_cancel,
    )?;
    assert_eq!(outcome.kind, InputOutcomeKind::CancelledAfterEffect);
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(key_events(&backend.events()), vec![(36, true), (36, false)]);
    Ok(())
}

#[test]
fn standalone_key_deadline_during_atomic_effect_is_reported()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.delay_on_pointer(1, Duration::from_millis(5));
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let outcome = engine.execute_operation(
        ActionContext::new(
            CommandId::new(),
            Some(Instant::now() + Duration::from_millis(1)),
        ),
        InputOperation::Keyboard(KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?),
        &CancellationToken::new(),
    )?;
    assert_eq!(outcome.kind, InputOutcomeKind::DeadlineExceededAfterEffect);
    assert_eq!(outcome.completed_units, 1);
    Ok(())
}

#[test]
fn sent_press_ledger_releases_key_and_button_missing_from_input_state()
-> Result<(), Box<dyn std::error::Error>> {
    let key_backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut key_engine = InputEngine::new_with_keyboard(key_backend.clone(), keyboard.boxed())?;
    key_engine.fail_next_state_mutation_for_test();
    let failure = failed(execute_keyboard_action(
        &mut key_engine,
        KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert!(key_engine.emergency_cleanup_after_panic());
    assert_eq!(
        key_events(&key_backend.events()),
        vec![(36, true), (36, false)]
    );

    let button_backend = MockBackend::new(RootPoint::new(0, 0)?);
    let mut button_engine = InputEngine::new(button_backend.clone())?;
    button_engine.fail_next_state_mutation_for_test();
    let button = xenoteer_core::input::PhysicalButton::new(1)?;
    let failure = failed(button_engine.execute(
        context(),
        InputAction::Button {
            button,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        },
        &CancellationToken::new(),
    ))?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert!(button_engine.emergency_cleanup_after_panic());
    assert!(matches!(
        button_backend.events().as_slice(),
        [
            BackendEvent::Button { pressed: true, .. },
            BackendEvent::Button { pressed: false, .. }
        ]
    ));
    Ok(())
}

#[test]
fn actor_panic_after_key_serialization_releases_before_terminal_reply()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.panic_after_send(1);
    let keyboard = MockKeyboardModel::new();
    let (handle, join) = spawn_test_actor_with_keyboard(2, {
        let backend = backend.clone();
        let keyboard = keyboard.clone();
        move || Ok((backend, keyboard.boxed()))
    })?;
    let reply = handle.try_submit_keyboard(
        context(),
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
        CancellationToken::new(),
    )?;
    let failure = failed(reply.blocking_recv()?)?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);
    assert_eq!(key_events(&backend.events()), vec![(36, true), (36, false)]);
    assert_eq!(join.join(), InputActorExit::Panicked);
    Ok(())
}

#[test]
fn actor_panic_with_temporary_mapping_releases_key_before_exact_restore()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let original = backend
        .keyboard_mapping(200)
        .ok_or_else(|| std::io::Error::other("missing original mapping"))?;
    backend.panic_after_send(1);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let (handle, join) = spawn_test_actor_with_keyboard(2, {
        let backend = backend.clone();
        let keyboard = keyboard.clone();
        move || Ok((backend, keyboard.boxed()))
    })?;
    let reply = handle.try_submit_keyboard(
        context(),
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
        CancellationToken::new(),
    )?;
    let failure = failed(reply.blocking_recv()?)?;
    assert_eq!(failure.kind, InputFailureKind::ActorPanicked);

    let operations = backend.operations();
    let release = operations
        .iter()
        .position(|operation| {
            matches!(
                operation,
                MockOperation::Event(BackendEvent::Key {
                    key,
                    pressed: false,
                    ..
                }) if key.keycode() == 200
            )
        })
        .ok_or_else(|| std::io::Error::other("missing emergency temporary-key release"))?;
    let restore = operations
        .iter()
        .position(|operation| {
            matches!(operation, MockOperation::MappingWrite(mapping) if *mapping == original)
        })
        .ok_or_else(|| std::io::Error::other("missing exact temporary-map restore"))?;
    assert!(release < restore);
    assert_eq!(backend.keyboard_mapping(200), Some(original));
    assert_eq!(join.join(), InputActorExit::Panicked);
    Ok(())
}

#[test]
fn send_error_ledger_only_uncertainty_is_released_and_query_proved()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_send(1, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::CheckedRequestFailed);
    assert_eq!(key_events(&backend.events()), vec![(36, false)]);
    let cleanup = failure
        .cleanup
        .as_deref()
        .and_then(super::InputCleanupEvidence::detailed)
        .ok_or_else(|| std::io::Error::other("missing detailed named-key cleanup"))?;
    assert!(cleanup.succeeded);
    assert_eq!(cleanup.attempted, 1);
    assert!(cleanup.residual_owned_keys.is_empty());
    assert!(cleanup.observed_pressed_keys.is_some());
    Ok(())
}

#[test]
fn keyboard_send_cookie_barrier_and_querykeymap_failures_cleanup_in_reverse()
-> Result<(), Box<dyn std::error::Error>> {
    for stage in 0..4 {
        let backend = MockBackend::new(RootPoint::new(0, 0)?);
        match stage {
            0 => backend.fail_send(2, BackendFaultKind::Request),
            1 => backend.fail_check(1, BackendFaultKind::Request),
            2 => backend.fail_pointer(1, BackendFaultKind::Request),
            3 => backend.fail_key_observation(1, BackendFaultKind::Request),
            _ => unreachable!(),
        }
        let keyboard = MockKeyboardModel::new();
        let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
        let failure = failed(execute_keyboard_action(
            &mut engine,
            KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
        ))?;
        assert!(matches!(
            failure.kind,
            InputFailureKind::CheckedRequestFailed | InputFailureKind::BarrierFailed
        ));
        assert_eq!(key_events(&backend.events()).last(), Some(&(36, false)));
        assert!(
            failure
                .cleanup
                .as_ref()
                .is_some_and(|cleanup| cleanup.succeeded())
        );
        assert_eq!(engine.snapshot_for_test().input, InputHealth::Healthy);
    }
    Ok(())
}

#[test]
fn query_keymap_connection_failure_is_terminal_without_false_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_key_observation(1, BackendFaultKind::Connection);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::BackendUnavailable);
    assert_eq!(
        key_events(&backend.events()),
        vec![(36, true), (36, false), (36, false)]
    );
    assert_eq!(
        engine.snapshot_for_test().input,
        InputHealth::Poisoned(PoisonReason::ConnectionLost)
    );
    assert!(
        failure
            .cleanup
            .as_ref()
            .is_some_and(|cleanup| cleanup.succeeded())
    );
    Ok(())
}

#[test]
fn mapping_change_before_and_after_keyboard_effects_are_distinct()
-> Result<(), Box<dyn std::error::Error>> {
    let before_backend = MockBackend::new(RootPoint::new(0, 0)?);
    let before_keyboard = MockKeyboardModel::new();
    before_keyboard.fail_next_validate(KeyboardModelFaultKind::MappingChanged);
    let mut before_engine =
        InputEngine::new_with_keyboard(before_backend.clone(), before_keyboard.boxed())?;
    let before = failed(execute_keyboard_action(
        &mut before_engine,
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
    ))?;
    assert_eq!(
        before.kind,
        InputFailureKind::KeyboardMappingChangedBeforeEffect
    );
    assert!(before_backend.events().is_empty());

    let after_backend = MockBackend::new(RootPoint::new(0, 0)?);
    let after_keyboard = MockKeyboardModel::new();
    let mut after_engine =
        InputEngine::new_with_keyboard(after_backend.clone(), after_keyboard.boxed())?;
    after_keyboard.enqueue_synchronized_generation(2);
    let after = failed(execute_keyboard_action(
        &mut after_engine,
        KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 0)?,
    ))?;
    assert_eq!(
        after.kind,
        InputFailureKind::KeyboardMappingChangedAfterEffect
    );
    assert_eq!(after.events_emitted, 2);
    assert!(
        after
            .cleanup
            .as_ref()
            .is_some_and(|cleanup| cleanup.succeeded())
    );
    Ok(())
}

#[test]
fn held_validation_conflict_releases_exact_key_and_preserves_failure_kind()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    execute_keyboard_action(
        &mut engine,
        KeyboardAction::down(KeyIdentifier::Named(NamedKey::Enter))?,
    )?;
    keyboard.fail_next_held_validation(KeyboardModelFaultKind::Conflict);
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::up(KeyIdentifier::Named(NamedKey::Enter))?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::ModifierConflictAfterEffect);
    assert_eq!(key_events(&backend.events()), vec![(36, true), (36, false)]);
    Ok(())
}

#[test]
fn temporary_strategy_rejects_nonempty_snapshot_and_missing_reservation()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.set_keyboard_mapping(
        200,
        CoreKeyboardMapping {
            keysyms_per_keycode: 4,
            keysyms: vec![1, 0, 0, 0],
        },
    );
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let nonempty = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(nonempty.kind, InputFailureKind::UnsupportedByBackend);
    assert!(backend.operations().is_empty());

    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    keyboard.fail_next_reservation(KeyboardModelFaultKind::NotRepresentable);
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let unavailable = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        unavailable.kind,
        InputFailureKind::TemporaryMappingInstallFailed
    );
    Ok(())
}

#[test]
fn temporary_install_and_restore_readback_failures_are_distinct()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.override_keyboard_mapping_read(
        2,
        CoreKeyboardMapping {
            keysyms_per_keycode: 4,
            keysyms: vec![0; 4],
        },
    );
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let install = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        install.kind,
        InputFailureKind::TemporaryMappingInstallFailed
    );

    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.override_keyboard_mapping_read(
        3,
        CoreKeyboardMapping {
            keysyms_per_keycode: 4,
            keysyms: vec![0x0101_f575, 0, 0, 0],
        },
    );
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let restore = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        restore.kind,
        InputFailureKind::TemporaryMappingRestoreFailed
    );
    Ok(())
}

#[test]
fn temporary_post_press_failure_releases_before_exact_restore()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    backend.fail_send(2, BackendFaultKind::Request);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    let mut engine = InputEngine::new_with_keyboard(backend.clone(), keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(failure.kind, InputFailureKind::CheckedRequestFailed);
    let operations = backend.operations();
    let cleanup_release = operations
        .iter()
        .rposition(|operation| {
            matches!(
                operation,
                MockOperation::Event(BackendEvent::Key { pressed: false, .. })
            )
        })
        .ok_or_else(|| std::io::Error::other("missing temporary-key cleanup release"))?;
    let restore_write = operations
        .iter()
        .rposition(|operation| matches!(operation, MockOperation::MappingWrite(mapping) if mapping.keysyms.iter().all(|keysym| *keysym == 0)))
        .ok_or_else(|| std::io::Error::other("missing exact mapping restore"))?;
    assert!(cleanup_release < restore_write);
    Ok(())
}

#[test]
fn temporary_model_restore_failure_redacts_pending_health_fingerprint()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let keyboard = MockKeyboardModel::new();
    keyboard.allow_reserved_scalar();
    keyboard.fail_synchronize_on_call(5, KeyboardModelFaultKind::Platform);
    keyboard.fail_synchronize_on_call(6, KeyboardModelFaultKind::Platform);
    let mut engine = InputEngine::new_with_keyboard(backend, keyboard.boxed())?;
    let failure = failed(execute_keyboard_action(
        &mut engine,
        KeyboardAction::physical_text("🕵", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
    ))?;
    assert_eq!(
        failure.kind,
        InputFailureKind::TemporaryMappingRestoreFailed
    );
    assert!(
        engine
            .snapshot_for_test()
            .keyboard_model
            .keymap_fingerprint
            .is_none()
    );
    assert!(
        failure
            .keyboard
            .as_ref()
            .is_some_and(|evidence| evidence.model.keymap_fingerprint.is_none())
    );
    Ok(())
}

fn event_delay(event: &BackendEvent) -> u32 {
    match *event {
        BackendEvent::Motion { delay_ms, .. }
        | BackendEvent::Button { delay_ms, .. }
        | BackendEvent::Key { delay_ms, .. } => delay_ms,
    }
}
