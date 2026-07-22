//! Serialized action execution, checked-request evidence, and conservative reset.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::RwLock;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use xenoteer_core::input::{
    ActionPurpose, ButtonDirection, CleanupAction, ClickAction, DragAction, EffectJournal,
    HealthEvent, InputAction, InputHealth, InputState, InputStateError, LogicalButton, MoveAction,
    PhysicalButton, ResetReason, plan_cleanup, plan_motion,
};

use crate::keyboard::{KeyIdentifier, KeyboardResolutionContext, KeyboardResolutionIntent};

use super::backend::{
    BackendEvent, BackendFault, BackendFaultKind, CoreKeyboardMapping, DrainedEvents, InputBackend,
    PointerObservation,
};
use super::keyboard_action::{KeyboardActionKind, KeyboardSequenceStep, PhysicalTextMode};
#[cfg(test)]
use super::keyboard_model::unavailable_keyboard_model;
use super::keyboard_model::{
    ActorKeyboardModel, CapturedKeyBinding, HeldBindingGeneration, KeyboardModelFault,
    KeyboardModelFaultKind, ModelPreflight, RequiredModifierBinding,
};
use super::{
    ActionContext, ActorThreadState, CleanupReport, InputCleanupEvidence, InputEffectEvidence,
    InputFailure, InputFailureKind, InputHealthSnapshot, InputOperation, InputOutcome,
    InputOutcomeKind, InputPrecondition, InputPreconditionFailure, KeyboardAction,
    KeyboardBindingEvidence, KeyboardOutcomeEvidence, PointerClickRequest, PointerDragRequest,
    PointerEndpoint, PointerMoveRelativeRequest, PointerMoveRequest, WindowPointerBoundsPolicy,
    WindowPointerClickRequest,
};

#[derive(Debug, Clone, Copy)]
enum StateEffect {
    Motion(xenoteer_core::domain::RootPoint),
    ButtonPress {
        button: PhysicalButton,
        allow_redundant: bool,
    },
    ButtonRelease {
        button: PhysicalButton,
        allow_redundant: bool,
    },
    KeyRelease(xenoteer_core::input::PhysicalKey),
    KeyPress {
        key: xenoteer_core::input::PhysicalKey,
        modifier: bool,
    },
    UntrackedRelease,
}

struct CapturedKeyHold {
    identifier: crate::keyboard::KeyIdentifier,
    binding: CapturedKeyBinding,
    synthesized_modifiers: Vec<xenoteer_core::input::PhysicalKey>,
}

#[derive(Debug, Clone, Copy)]
struct SynthesizedModifierRef {
    key: xenoteer_core::input::PhysicalKey,
    references: u16,
}

#[derive(Debug, Clone)]
struct PendingTemporaryRestore {
    key: xenoteer_core::input::PhysicalKey,
    original: CoreKeyboardMapping,
}

#[derive(Debug, Default)]
struct SentPressLedger {
    keys: [u64; 4],
    modifier_keys: [u64; 4],
    buttons: [u64; 4],
}

impl SentPressLedger {
    fn note(&mut self, event: BackendEvent, effect: StateEffect) {
        match (event, effect) {
            (
                BackendEvent::Key {
                    key, pressed: true, ..
                },
                StateEffect::KeyPress { modifier, .. },
            ) => {
                set_bit(&mut self.keys, key.keycode(), true);
                if modifier {
                    set_bit(&mut self.modifier_keys, key.keycode(), true);
                }
            }
            (
                BackendEvent::Button {
                    button,
                    pressed: true,
                    ..
                },
                StateEffect::ButtonPress { .. },
            ) => {
                set_bit(&mut self.buttons, button.detail(), true);
            }
            _ => {}
        }
    }

    fn reconcile_keys(&mut self, pressed: &[xenoteer_core::input::PhysicalKey]) {
        for detail in 8_u8..=u8::MAX {
            if bit_is_set(&self.keys, detail) && !pressed.iter().any(|key| key.keycode() == detail)
            {
                set_bit(&mut self.keys, detail, false);
                set_bit(&mut self.modifier_keys, detail, false);
            }
        }
    }

    fn reconcile_buttons(
        &mut self,
        mapping: &xenoteer_core::input::ButtonMapping,
        logical_buttons: [bool; 5],
    ) {
        for detail in 1_u8..=u8::MAX {
            if !bit_is_set(&self.buttons, detail) {
                continue;
            }
            let Ok(button) = PhysicalButton::new(detail) else {
                continue;
            };
            if logical_for_physical(mapping, button).is_some_and(|logical| {
                (1..=5).contains(&logical)
                    && !logical_buttons[usize::from(logical.saturating_sub(1))]
            }) {
                set_bit(&mut self.buttons, detail, false);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PlannedEvent {
    backend: BackendEvent,
    effect: StateEffect,
}

#[derive(Debug)]
struct BatchSuccess {
    attempted: usize,
    sent: usize,
    observation: PointerObservation,
    mapping_changed: bool,
}

#[derive(Debug, Clone, Copy)]
enum BatchFailureKind {
    Connection,
    CheckedRequest,
    Barrier,
    MappingChanged,
    State,
}

#[derive(Debug)]
struct BatchFailure {
    kind: BatchFailureKind,
    attempted: usize,
    sent: usize,
    observation: Option<PointerObservation>,
}

impl BatchFailure {
    const fn public_kind(&self) -> InputFailureKind {
        match self.kind {
            BatchFailureKind::Connection => InputFailureKind::BackendUnavailable,
            BatchFailureKind::CheckedRequest => InputFailureKind::CheckedRequestFailed,
            BatchFailureKind::Barrier => InputFailureKind::BarrierFailed,
            BatchFailureKind::MappingChanged => InputFailureKind::ButtonMappingChangedAfterEffect,
            BatchFailureKind::State => InputFailureKind::ActorPanicked,
        }
    }
}

#[derive(Debug)]
struct ActionError {
    kind: InputFailureKind,
    events_emitted: usize,
    completed_units: u16,
    requested_pointer: Option<xenoteer_core::domain::RootPoint>,
    last_observed_pointer: Option<xenoteer_core::domain::RootPoint>,
    observed_buttons: Option<[bool; 5]>,
    button_observation_partial: bool,
    progress_known: bool,
}

#[derive(Debug, Default)]
struct ActionProgress {
    events_emitted: usize,
    completed_units: u16,
    requested_pointer: Option<xenoteer_core::domain::RootPoint>,
    observed_pointer: Option<xenoteer_core::domain::RootPoint>,
    observed_buttons: Option<[bool; 5]>,
    button_observation_partial: bool,
    stopped: Option<BoundaryStop>,
}

struct WindowClickRevalidation {
    request: WindowPointerClickRequest,
    expected_target: xenoteer_core::domain::RootPoint,
    precondition: Option<InputPrecondition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryStop {
    Cancelled,
    Deadline,
}

#[derive(Debug, Default)]
struct KeyboardProgress {
    events_emitted: usize,
    completed_units: u16,
    stopped: Option<BoundaryStop>,
    bindings: Vec<KeyboardBindingEvidence>,
    text_scalar_count: Option<usize>,
    requested_text_mode: Option<PhysicalTextMode>,
    current_layout_scalars: usize,
    temporary_mapping_scalars: usize,
    temporary_mappings_installed: usize,
    temporary_mappings_restored: usize,
    temporary_mapping_restoration_proven: Option<bool>,
    redact_scalar_evidence: bool,
    pointer: Option<PointerObservation>,
}

struct ResolvedStepContext<'a> {
    resolution: &'a KeyboardResolutionContext,
    cancellation: &'a CancellationToken,
    deadline: Option<Instant>,
    record_binding_evidence: bool,
}

struct CleanupReportInputs {
    attempted: usize,
    confirmed: usize,
    pointer: Option<PointerObservation>,
    keys: Option<Vec<xenoteer_core::input::PhysicalKey>>,
    unobservable_buttons: Vec<PhysicalButton>,
    succeeded: bool,
    temporary_mapping_restore_attempted: bool,
    temporary_mapping_restore_proven: bool,
}

#[derive(Debug)]
struct KeyboardRunError {
    kind: InputFailureKind,
    progress: KeyboardProgress,
    progress_known: bool,
}

#[derive(Debug, Clone, Copy)]
struct KeyBatchPostflight {
    model: ModelPreflight,
    accepted_initial_xtest_set_map: bool,
}

impl KeyboardProgress {
    fn failure(self, kind: InputFailureKind) -> KeyboardRunError {
        KeyboardRunError {
            progress_known: kind != InputFailureKind::ActorPanicked,
            kind,
            progress: self,
        }
    }
}

pub(super) struct InputEngine<B: InputBackend> {
    backend: B,
    state: InputState,
    button_mapping: xenoteer_core::input::ButtonMapping,
    min_keycode: u8,
    max_keycode: u8,
    last_pointer: Option<xenoteer_core::domain::RootPoint>,
    invariant_failed: bool,
    keyboard: Box<dyn ActorKeyboardModel>,
    captured_keys: Vec<CapturedKeyHold>,
    synthesized_modifiers: Vec<SynthesizedModifierRef>,
    pending_keyboard_restore: Option<PendingTemporaryRestore>,
    sent_press_ledger: SentPressLedger,
    keyboard_effect_emitted: bool,
    #[cfg(test)]
    fail_next_state_mutation: bool,
}

impl<B: InputBackend> InputEngine<B> {
    #[cfg(test)]
    pub(super) fn new(backend: B) -> Result<Self, BackendFault> {
        Self::new_with_keyboard(backend, unavailable_keyboard_model())
    }

    pub(super) fn new_with_keyboard(
        backend: B,
        mut keyboard: Box<dyn ActorKeyboardModel>,
    ) -> Result<Self, BackendFault> {
        let startup = backend.startup()?;
        // Xvfb can broadcast a core keyboard map and then a modifier map while
        // its initial XKB model settles. Require two consecutive ordered,
        // clean preflights at one generation after the XTEST connection has
        // completed startup; a single first-clean return can precede the
        // second broadcast on the model's independent connection.
        let mut stable_generation = None;
        let mut startup_stable = false;
        for _round in 0..8 {
            match keyboard.synchronize_preflight() {
                Ok(preflight) => {
                    if stable_generation == Some(preflight.generation) {
                        startup_stable = true;
                        break;
                    }
                    stable_generation = Some(preflight.generation);
                }
                Err(fault) if fault.kind == KeyboardModelFaultKind::Unavailable => {
                    startup_stable = true;
                    break;
                }
                Err(fault) => {
                    let kind = if fault.kind.is_connection() {
                        BackendFaultKind::Connection
                    } else {
                        BackendFaultKind::Capability
                    };
                    return Err(BackendFault::new(
                        kind,
                        "keyboard model startup synchronization failed",
                    ));
                }
            }
        }
        if !startup_stable {
            return Err(BackendFault::new(
                BackendFaultKind::Capability,
                "keyboard model did not stabilize during actor startup",
            ));
        }
        Ok(Self {
            backend,
            state: InputState::new(),
            button_mapping: startup.button_mapping,
            min_keycode: startup.min_keycode,
            max_keycode: startup.max_keycode,
            last_pointer: None,
            invariant_failed: false,
            keyboard,
            captured_keys: Vec::new(),
            synthesized_modifiers: Vec::new(),
            pending_keyboard_restore: None,
            sent_press_ledger: SentPressLedger::default(),
            keyboard_effect_emitted: false,
            #[cfg(test)]
            fail_next_state_mutation: false,
        })
    }

    pub(super) fn publish_health(
        &self,
        target: &RwLock<InputHealthSnapshot>,
        thread: ActorThreadState,
    ) {
        let mut snapshot = target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.input = self.state.health();
        snapshot.thread = thread;
        snapshot.button_mapping = Some(self.button_mapping.clone());
        snapshot.min_keycode = self.min_keycode;
        snapshot.max_keycode = self.max_keycode;
        snapshot.keyboard_model = self.keyboard_diagnostics(false);
    }

    pub(super) fn mark_panicked(&mut self) {
        self.invariant_failed = true;
        let _ignored = self.state.transition_health(HealthEvent::ActorPanicked);
    }

    pub(super) fn actor_panicked(&self) -> bool {
        self.invariant_failed
            || matches!(
                self.state.health(),
                InputHealth::Poisoned(xenoteer_core::input::PoisonReason::ActorPanicked)
            )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn execute(
        &mut self,
        context: ActionContext,
        action: InputAction,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        self.execute_with_precondition(context, action, None, cancellation)
    }

    fn execute_with_precondition(
        &mut self,
        context: ActionContext,
        action: InputAction,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        self.execute_prepared_action(context, action, precondition, None, cancellation)
    }

    fn execute_prepared_action(
        &mut self,
        context: ActionContext,
        action: InputAction,
        precondition: Option<InputPrecondition>,
        mut window_click: Option<WindowClickRevalidation>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        let requested_pointer = requested_pointer_action(&action);
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                requested_pointer,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                requested_pointer,
            ));
        }
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.failure_before(
                context,
                backend_public_kind(&fault),
                requested_pointer,
            ));
        }
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                requested_pointer,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                requested_pointer,
            ));
        }
        if self.state.health() != InputHealth::Healthy {
            return Err(self.failure_before(
                context,
                InputFailureKind::HealthRejected,
                requested_pointer,
            ));
        }
        if let InputAction::Key(_) = action {
            return Err(self.failure_before(
                context,
                InputFailureKind::UnsupportedOperation,
                requested_pointer,
            ));
        }
        if let Err(kind) = self.preflight(&action) {
            return Err(self.failure_before(context, kind, requested_pointer));
        }
        self.check_precondition(context, precondition, requested_pointer)?;
        if self.state.begin_action(ActionPurpose::Ordinary).is_err() {
            self.mark_panicked();
            let _abandoned_journal = self.state.finish_action();
            return Err(self.invariant_failure(context, requested_pointer, None));
        }

        let result = self.run_action(
            &action,
            cancellation,
            context.deadline,
            window_click.as_mut(),
        );
        match result {
            Ok(progress) => {
                let effects = match self.state.finish_action() {
                    Ok(effects) => effects,
                    Err(_) => {
                        self.mark_panicked();
                        let effects = self.state.finish_action().ok();
                        return Err(self.finish_failure(
                            context,
                            ActionError {
                                kind: InputFailureKind::ActorPanicked,
                                events_emitted: progress.events_emitted,
                                completed_units: progress.completed_units,
                                requested_pointer: progress.requested_pointer,
                                last_observed_pointer: progress.observed_pointer,
                                observed_buttons: progress.observed_buttons,
                                button_observation_partial: progress.button_observation_partial,
                                progress_known: false,
                            },
                            effects.map(|journal| (journal, None)),
                        ));
                    }
                };
                let kind = match (progress.stopped, progress.events_emitted) {
                    (Some(BoundaryStop::Cancelled), 0) => {
                        return Err(InputFailure {
                            command_id: Some(context.command_id),
                            kind: InputFailureKind::CancelledBeforeEffect,
                            events_emitted: 0,
                            completed_units: 0,
                            progress_known: true,
                            requested_pointer: progress.requested_pointer,
                            last_observed_pointer: progress.observed_pointer,
                            observed_logical_buttons_1_to_5: progress.observed_buttons,
                            button_observation_partial: progress.button_observation_partial,
                            effects: Some(Box::new(effects.into())),
                            cleanup: None,
                            keyboard: None,
                        });
                    }
                    (Some(BoundaryStop::Deadline), 0) => {
                        return Err(InputFailure {
                            command_id: Some(context.command_id),
                            kind: InputFailureKind::DeadlineExceededBeforeEffect,
                            events_emitted: 0,
                            completed_units: 0,
                            progress_known: true,
                            requested_pointer: progress.requested_pointer,
                            last_observed_pointer: progress.observed_pointer,
                            observed_logical_buttons_1_to_5: progress.observed_buttons,
                            button_observation_partial: progress.button_observation_partial,
                            effects: Some(Box::new(effects.into())),
                            cleanup: None,
                            keyboard: None,
                        });
                    }
                    (Some(BoundaryStop::Cancelled), _) => InputOutcomeKind::CancelledAfterEffect,
                    (Some(BoundaryStop::Deadline), _) => {
                        InputOutcomeKind::DeadlineExceededAfterEffect
                    }
                    (None, _) => InputOutcomeKind::Completed,
                };
                Ok(InputOutcome {
                    command_id: context.command_id,
                    kind,
                    events_emitted: progress.events_emitted,
                    completed_units: progress.completed_units,
                    requested_pointer: progress.requested_pointer,
                    observed_pointer: progress.observed_pointer,
                    observed_logical_buttons_1_to_5: progress.observed_buttons,
                    button_observation_partial: progress.button_observation_partial,
                    effects: effects.into(),
                    keyboard: None,
                })
            }
            Err(error) => {
                let effects = match self.state.finish_action() {
                    Ok(effects) => effects,
                    Err(_) => {
                        self.mark_panicked();
                        let effects = self.state.finish_action().ok();
                        return Err(self.finish_failure(
                            context,
                            ActionError {
                                kind: InputFailureKind::ActorPanicked,
                                progress_known: false,
                                ..error
                            },
                            effects.map(|journal| (journal, None)),
                        ));
                    }
                };
                if error.kind == InputFailureKind::ActorPanicked {
                    return Err(self.finish_failure(context, error, Some((effects, None))));
                }
                if self.state.health() == InputHealth::Healthy {
                    return Err(self.finish_failure(context, error, Some((effects, None))));
                }
                let cleanup = match self.reset_owned_input() {
                    Ok(report) => Some(Box::new(report.into())),
                    Err(failure) => failure.cleanup,
                };
                Err(self.finish_failure(context, error, Some((effects, cleanup))))
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn execute_operation(
        &mut self,
        context: ActionContext,
        operation: InputOperation,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        self.execute_operation_with_precondition(context, operation, None, cancellation)
    }

    pub(super) fn execute_operation_with_precondition(
        &mut self,
        context: ActionContext,
        operation: InputOperation,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        match operation {
            InputOperation::Pointer(action) => {
                self.execute_with_precondition(context, action, precondition, cancellation)
            }
            InputOperation::PointerMove(request) => {
                self.execute_pointer_move(context, request, precondition, cancellation)
            }
            InputOperation::PointerMoveRelative(request) => {
                self.execute_pointer_move_relative(context, request, precondition, cancellation)
            }
            InputOperation::PointerClick(request) => {
                self.execute_pointer_click(context, request, precondition, cancellation)
            }
            InputOperation::PointerDrag(request) => {
                self.execute_pointer_drag(context, request, precondition, cancellation)
            }
            InputOperation::WindowPointerClick(request) => {
                self.execute_window_pointer_click(context, request, precondition, cancellation)
            }
            InputOperation::PointerScroll(action) => self.execute_with_precondition(
                context,
                InputAction::Scroll(action),
                precondition,
                cancellation,
            ),
            InputOperation::Keyboard(action) => {
                self.execute_keyboard(context, action, precondition, cancellation)
            }
        }
    }

    fn execute_pointer_move(
        &mut self,
        context: ActionContext,
        request: PointerMoveRequest,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        let target = Some(request.target());
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                target,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                target,
            ));
        }
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.failure_before(context, backend_public_kind(&fault), target));
        }
        if self.state.health() != InputHealth::Healthy {
            return Err(self.failure_before(context, InputFailureKind::HealthRejected, target));
        }
        let start = match self.backend.observe_pointer() {
            Ok(observation) => {
                self.last_pointer = Some(observation.pointer);
                observation.pointer
            }
            Err(fault) => {
                self.apply_backend_fault(&fault);
                return Err(self.failure_before(context, backend_public_kind(&fault), target));
            }
        };
        let plan = plan_motion(start, request.target(), request.options())
            .map_err(|_| self.failure_before(context, InputFailureKind::StateRejected, target))?;
        self.execute_with_precondition(
            context,
            InputAction::Move(MoveAction::new(plan)),
            precondition,
            cancellation,
        )
    }

    fn execute_pointer_move_relative(
        &mut self,
        context: ActionContext,
        request: PointerMoveRelativeRequest,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        let (start, target) = self.resolve_pointer_endpoint(
            context,
            PointerEndpoint::Relative(request.delta()),
            cancellation,
        )?;
        let plan = plan_motion(start, target, request.options()).map_err(|_| {
            self.failure_before(context, InputFailureKind::StateRejected, Some(target))
        })?;
        self.execute_with_precondition(
            context,
            InputAction::Move(MoveAction::new(plan)),
            precondition,
            cancellation,
        )
    }

    fn execute_pointer_click(
        &mut self,
        context: ActionContext,
        request: PointerClickRequest,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        let movement = match request.endpoint() {
            Some(endpoint) => {
                let (start, target) =
                    self.resolve_pointer_endpoint(context, endpoint, cancellation)?;
                Some(plan_motion(start, target, request.options()).map_err(|_| {
                    self.failure_before(context, InputFailureKind::StateRejected, Some(target))
                })?)
            }
            None => None,
        };
        let target = movement.as_ref().map(|plan| plan.end());
        let action = ClickAction::new(
            movement,
            request.button(),
            request.count(),
            request.pre_click_dwell_ms(),
            request.press_duration_ms(),
            request.inter_click_interval_ms(),
            xenoteer_core::input::DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
        )
        .map_err(|_| self.failure_before(context, InputFailureKind::StateRejected, target))?;
        self.execute_with_precondition(
            context,
            InputAction::Click(action),
            precondition,
            cancellation,
        )
    }

    fn execute_pointer_drag(
        &mut self,
        context: ActionContext,
        request: PointerDragRequest,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        let (start, target) =
            self.resolve_pointer_endpoint(context, request.endpoint(), cancellation)?;
        let movement = plan_motion(start, target, request.options()).map_err(|_| {
            self.failure_before(context, InputFailureKind::StateRejected, Some(target))
        })?;
        let action = DragAction::new(
            movement,
            request.button(),
            request.press_dwell_ms(),
            request.release_dwell_ms(),
        )
        .map_err(|_| self.failure_before(context, InputFailureKind::StateRejected, Some(target)))?;
        self.execute_with_precondition(
            context,
            InputAction::Drag(action),
            precondition,
            cancellation,
        )
    }

    fn execute_window_pointer_click(
        &mut self,
        context: ActionContext,
        request: WindowPointerClickRequest,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                None,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                None,
            ));
        }
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.failure_before(context, backend_public_kind(&fault), None));
        }
        if self.state.health() != InputHealth::Healthy {
            return Err(self.failure_before(context, InputFailureKind::HealthRejected, None));
        }
        let target = self
            .resolve_window_click_target(request)
            .map_err(|kind| self.failure_before(context, kind, None))?;
        let start = self
            .backend
            .observe_pointer()
            .map_err(|fault| {
                self.apply_backend_fault(&fault);
                self.failure_before(context, backend_public_kind(&fault), Some(target))
            })?
            .pointer;
        self.last_pointer = Some(start);
        let movement = plan_motion(start, target, request.options()).map_err(|_| {
            self.failure_before(context, InputFailureKind::StateRejected, Some(target))
        })?;
        let action = ClickAction::new(
            Some(movement),
            request.button(),
            request.count(),
            request.pre_click_dwell_ms(),
            request.press_duration_ms(),
            request.inter_click_interval_ms(),
            xenoteer_core::input::DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
        )
        .map_err(|_| self.failure_before(context, InputFailureKind::StateRejected, Some(target)))?;
        self.execute_prepared_action(
            context,
            InputAction::Click(action),
            None,
            Some(WindowClickRevalidation {
                request,
                expected_target: target,
                precondition,
            }),
            cancellation,
        )
    }

    fn resolve_window_click_target(
        &mut self,
        request: WindowPointerClickRequest,
    ) -> Result<xenoteer_core::domain::RootPoint, InputFailureKind> {
        let geometry = self
            .backend
            .observe_window_geometry(request.window())
            .map_err(|fault| {
                self.apply_backend_fault(&fault);
                backend_public_kind(&fault)
            })?;
        let selected = match request.coordinate_space() {
            xenoteer_protocol::CoordinateSpace::WindowClient => geometry.window().client_rect,
            xenoteer_protocol::CoordinateSpace::WindowFrame => geometry
                .window()
                .frame_rect
                .ok_or(InputFailureKind::UnsupportedByBackend)?,
            xenoteer_protocol::CoordinateSpace::RootPhysical
            | xenoteer_protocol::CoordinateSpace::AtspiScreen => {
                return Err(InputFailureKind::StateRejected);
            }
        };
        let size = selected
            .rect
            .size()
            .map_err(|_| InputFailureKind::StateRejected)?;
        let requested = request.point();
        let inside = requested.x() >= 0
            && requested.y() >= 0
            && i64::from(requested.x()) < i64::from(size.width())
            && i64::from(requested.y()) < i64::from(size.height());
        let local = match request.bounds_policy() {
            WindowPointerBoundsPolicy::Reject if !inside => {
                return Err(InputFailureKind::StateRejected);
            }
            WindowPointerBoundsPolicy::Clamp => xenoteer_protocol::Point::new(
                requested
                    .x()
                    .clamp(0, i32::try_from(size.width() - 1).unwrap_or(i32::MAX)),
                requested
                    .y()
                    .clamp(0, i32::try_from(size.height() - 1).unwrap_or(i32::MAX)),
            ),
            WindowPointerBoundsPolicy::Reject | WindowPointerBoundsPolicy::Allow => requested,
        };
        let resolved = geometry
            .resolve_local_point(
                request.coordinate_space(),
                local,
                xenoteer_protocol::WindowScreenBoundsPolicy::AllowOffscreen,
            )
            .map_err(|error| match error {
                xenoteer_core::window_geometry::WindowGeometryResolveError::FrameGeometryUnavailable
                | xenoteer_core::window_geometry::WindowGeometryResolveError::UnsupportedCoordinateSpace => {
                    InputFailureKind::UnsupportedByBackend
                }
                _ => InputFailureKind::StateRejected,
            })?;
        xenoteer_core::domain::RootPoint::try_from_protocol(resolved.root)
            .map_err(|_| InputFailureKind::StateRejected)
    }

    fn resolve_pointer_endpoint(
        &mut self,
        context: ActionContext,
        endpoint: PointerEndpoint,
        cancellation: &CancellationToken,
    ) -> Result<
        (
            xenoteer_core::domain::RootPoint,
            xenoteer_core::domain::RootPoint,
        ),
        InputFailure,
    > {
        let requested = match endpoint {
            PointerEndpoint::Root(target) => Some(target),
            PointerEndpoint::Relative(_) => None,
        };
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                requested,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                requested,
            ));
        }
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.failure_before(context, backend_public_kind(&fault), requested));
        }
        if self.state.health() != InputHealth::Healthy {
            return Err(self.failure_before(context, InputFailureKind::HealthRejected, requested));
        }
        let start = self
            .backend
            .observe_pointer()
            .map_err(|fault| {
                self.apply_backend_fault(&fault);
                self.failure_before(context, backend_public_kind(&fault), requested)
            })?
            .pointer;
        self.last_pointer = Some(start);
        let target = match endpoint {
            PointerEndpoint::Root(target) => target,
            PointerEndpoint::Relative(delta) => start
                .checked_add(delta)
                .map_err(|_| self.failure_before(context, InputFailureKind::StateRejected, None))?,
        };
        Ok((start, target))
    }

    fn check_precondition(
        &self,
        context: ActionContext,
        precondition: Option<InputPrecondition>,
        requested_pointer: Option<xenoteer_core::domain::RootPoint>,
    ) -> Result<(), InputFailure> {
        let Some(mut precondition) = precondition else {
            return Ok(());
        };
        precondition.evaluate().map_err(|failure| {
            self.failure_before(
                context,
                input_precondition_failure_kind(failure),
                requested_pointer,
            )
        })
    }

    fn execute_keyboard(
        &mut self,
        context: ActionContext,
        action: KeyboardAction,
        precondition: Option<InputPrecondition>,
        cancellation: &CancellationToken,
    ) -> Result<InputOutcome, InputFailure> {
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                None,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                None,
            ));
        }
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.failure_before(context, backend_public_kind(&fault), None));
        }
        if cancellation.is_cancelled() {
            return Err(self.failure_before(
                context,
                InputFailureKind::CancelledBeforeEffect,
                None,
            ));
        }
        if deadline_elapsed(context.deadline) {
            return Err(self.failure_before(
                context,
                InputFailureKind::DeadlineExceededBeforeEffect,
                None,
            ));
        }
        if self.state.health() != InputHealth::Healthy || self.pending_keyboard_restore.is_some() {
            return Err(self.failure_before(context, InputFailureKind::HealthRejected, None));
        }
        self.check_precondition(context, precondition, None)?;
        if self.state.begin_action(ActionPurpose::Ordinary).is_err() {
            self.mark_panicked();
            let _abandoned_journal = self.state.finish_action();
            return Err(self.invariant_failure(context, None, None));
        }

        match self.run_keyboard_action(&action, cancellation, context.deadline) {
            Ok(progress) => {
                let effects = match self.state.finish_action() {
                    Ok(effects) => effects,
                    Err(_) => {
                        self.mark_panicked();
                        let effects = self.state.finish_action().ok();
                        return Err(self.keyboard_run_failure(
                            context,
                            KeyboardRunError {
                                kind: InputFailureKind::ActorPanicked,
                                progress,
                                progress_known: false,
                            },
                            effects,
                            None,
                        ));
                    }
                };
                let kind = match (progress.stopped, progress.events_emitted) {
                    (Some(BoundaryStop::Cancelled), 0) => {
                        return Err(self.keyboard_run_failure(
                            context,
                            progress.failure(InputFailureKind::CancelledBeforeEffect),
                            Some(effects),
                            None,
                        ));
                    }
                    (Some(BoundaryStop::Deadline), 0) => {
                        return Err(self.keyboard_run_failure(
                            context,
                            progress.failure(InputFailureKind::DeadlineExceededBeforeEffect),
                            Some(effects),
                            None,
                        ));
                    }
                    (Some(BoundaryStop::Cancelled), _) => InputOutcomeKind::CancelledAfterEffect,
                    (Some(BoundaryStop::Deadline), _) => {
                        InputOutcomeKind::DeadlineExceededAfterEffect
                    }
                    (None, _) => InputOutcomeKind::Completed,
                };
                let pointer = progress.pointer.clone();
                Ok(InputOutcome {
                    command_id: context.command_id,
                    kind,
                    events_emitted: progress.events_emitted,
                    completed_units: progress.completed_units,
                    requested_pointer: None,
                    observed_pointer: pointer.as_ref().map(|value| value.pointer),
                    observed_logical_buttons_1_to_5: pointer
                        .map(|value| value.logical_buttons_1_to_5),
                    button_observation_partial: false,
                    effects: InputEffectEvidence::from_journal(
                        effects,
                        progress.redact_scalar_evidence,
                    ),
                    keyboard: Some(Box::new(self.keyboard_outcome_evidence(progress))),
                })
            }
            Err(error) => {
                let effects = match self.state.finish_action() {
                    Ok(effects) => effects,
                    Err(_) => {
                        self.mark_panicked();
                        let effects = self.state.finish_action().ok();
                        return Err(self.keyboard_run_failure(
                            context,
                            KeyboardRunError {
                                kind: InputFailureKind::ActorPanicked,
                                progress: error.progress,
                                progress_known: false,
                            },
                            effects,
                            None,
                        ));
                    }
                };
                if error.kind == InputFailureKind::ActorPanicked
                    || self.state.health() == InputHealth::Healthy
                {
                    return Err(self.keyboard_run_failure(context, error, Some(effects), None));
                }
                let cleanup = match self.reset_owned_input() {
                    Ok(report) => Some(Box::new(report.into())),
                    Err(failure) => failure.cleanup,
                };
                Err(self.keyboard_run_failure(context, error, Some(effects), cleanup))
            }
        }
    }

    pub(super) fn probe(&mut self) -> Result<InputHealthSnapshot, InputFailure> {
        if let Err(fault) = self.drain_events() {
            self.apply_backend_fault(&fault);
            return Err(self.actor_failure(backend_public_kind(&fault), None));
        }
        if let Err(fault) = self.keyboard.synchronize_preflight()
            && fault.kind != KeyboardModelFaultKind::Unavailable
        {
            let kind = self.keyboard_model_failure(fault, false);
            return Err(self.actor_failure(kind, None));
        }
        match self.backend.observe_pointer() {
            Ok(observation) => self.last_pointer = Some(observation.pointer),
            Err(fault) => {
                let kind = if fault.kind == BackendFaultKind::Connection {
                    self.apply_backend_fault(&fault);
                    InputFailureKind::BackendUnavailable
                } else {
                    let _ignored = self
                        .state
                        .transition_health(HealthEvent::RequireReset(ResetReason::BarrierFailed));
                    InputFailureKind::BarrierFailed
                };
                return Err(self.actor_failure(kind, None));
            }
        }
        Ok(self.snapshot(ActorThreadState::Running))
    }

    pub(super) fn reset_owned_input(&mut self) -> Result<CleanupReport, InputFailure> {
        let health_before = self.state.health();
        if self.state.begin_action(ActionPurpose::Reset).is_err() {
            self.mark_panicked();
            let _abandoned_journal = self.state.finish_action();
            return Err(self.invariant_actor_failure(None));
        }

        let temporary_mapping_restore_attempted = self.pending_keyboard_restore.is_some();
        let mut temporary_mapping_restore_proven = false;
        let mut temporary_mapping_restore_failure = None;

        let cleanup = self.conservative_cleanup_actions();
        let state_release_count = plan_cleanup(&self.state).len();
        let owned_buttons: Vec<_> = cleanup
            .iter()
            .filter_map(|action| match action {
                CleanupAction::ReleaseButton { button } => Some(*button),
                CleanupAction::ReleaseKey { .. } => None,
            })
            .collect();
        let owned_keys: Vec<_> = cleanup
            .iter()
            .filter_map(|action| match action {
                CleanupAction::ReleaseKey { key, .. } => Some(*key),
                CleanupAction::ReleaseButton { .. } => None,
            })
            .collect();
        let planned: Vec<_> = cleanup
            .iter()
            .map(|action| match *action {
                CleanupAction::ReleaseButton { button } => PlannedEvent {
                    backend: BackendEvent::Button {
                        button,
                        pressed: false,
                        delay_ms: 0,
                    },
                    effect: if self.state.pressed_buttons().contains(&button) {
                        StateEffect::ButtonRelease {
                            button,
                            allow_redundant: false,
                        }
                    } else {
                        StateEffect::UntrackedRelease
                    },
                },
                CleanupAction::ReleaseKey { key, .. } => PlannedEvent {
                    backend: BackendEvent::Key {
                        key,
                        pressed: false,
                        delay_ms: 0,
                    },
                    effect: if self
                        .state
                        .pressed_keys()
                        .iter()
                        .any(|owned| owned.key() == key)
                    {
                        StateEffect::KeyRelease(key)
                    } else {
                        StateEffect::UntrackedRelease
                    },
                },
            })
            .collect();

        let batch = self.run_batch(&planned, !owned_buttons.is_empty(), false);
        let batch_invariant_failed = matches!(
            batch.as_ref().err().map(|error| error.kind),
            Some(BatchFailureKind::State)
        );
        if batch_invariant_failed {
            self.mark_panicked();
        }
        let attempted = match &batch {
            Ok(value) => value.attempted,
            Err(error) => error.attempted,
        };
        let pointer_evidence = match &batch {
            Ok(value) => Some(value.observation.clone()),
            Err(error) => error.observation.clone(),
        };
        let can_query_keys = !matches!(
            batch.as_ref().err().map(|error| error.kind),
            Some(BatchFailureKind::Connection | BatchFailureKind::State)
        );
        let mut key_fault = None;
        let key_evidence = if can_query_keys {
            match self.backend.observe_keys() {
                Ok(keys) => {
                    self.sent_press_ledger.reconcile_keys(&keys);
                    Some(keys)
                }
                Err(fault) => {
                    key_fault = Some(fault);
                    None
                }
            }
        } else {
            None
        };
        if let Some(fault) = &key_fault
            && fault.kind == BackendFaultKind::Connection
        {
            self.apply_backend_fault(fault);
        }

        let mut unobservable_buttons = Vec::new();
        let button_observation_valid = pointer_evidence.as_ref().is_some_and(|observation| {
            owned_buttons.iter().all(|button| {
                match logical_for_physical(&self.button_mapping, *button) {
                    Some(logical @ 1..=5) => {
                        !observation.logical_buttons_1_to_5[usize::from(logical - 1)]
                    }
                    _ => {
                        unobservable_buttons.push(*button);
                        true
                    }
                }
            })
        });
        let key_observation_valid = if owned_keys.is_empty() {
            true
        } else {
            key_evidence
                .as_ref()
                .is_some_and(|pressed| owned_keys.iter().all(|key| !pressed.contains(key)))
        };
        let request_evidence_valid = batch.is_ok();
        let key_request_valid = key_fault.as_ref().is_none_or(|fault| {
            owned_keys.is_empty() && fault.kind != BackendFaultKind::Connection
        });
        let observations_valid =
            button_observation_valid && key_observation_valid && key_request_valid;
        let mut confirmed_batch = false;
        let mut proof_succeeded = request_evidence_valid && observations_valid;
        let mut invariant_failed = batch_invariant_failed || self.actor_panicked();
        let poisoned_retry_failure =
            matches!(health_before, InputHealth::Poisoned(_)) && !proof_succeeded;
        let mut poisoned_batch_abandoned = false;
        if poisoned_retry_failure && state_release_count > 0 && !batch_invariant_failed {
            match self.state.abandon_poisoned_reset_batch() {
                Ok(()) => poisoned_batch_abandoned = true,
                Err(InputStateError::NoPendingBatch) if batch.is_err() => {
                    poisoned_batch_abandoned = true;
                }
                Err(_) => {
                    self.mark_panicked();
                    invariant_failed = true;
                }
            }
        }

        if proof_succeeded && state_release_count > 0 {
            if self.state.confirm_batch().is_ok() {
                confirmed_batch = true;
            } else {
                self.mark_panicked();
                proof_succeeded = false;
                invariant_failed = true;
            }
        } else if proof_succeeded {
            confirmed_batch = true;
        } else if poisoned_retry_failure && (poisoned_batch_abandoned || state_release_count == 0) {
        } else if request_evidence_valid && state_release_count > 0 {
            if self
                .state
                .fail_batch(ResetReason::PostconditionFailed)
                .is_err()
            {
                self.mark_panicked();
                invariant_failed = true;
            }
        } else if request_evidence_valid
            && state_release_count == 0
            && self
                .state
                .transition_health(HealthEvent::RequireReset(ResetReason::PostconditionFailed))
                .is_err()
        {
            self.mark_panicked();
            invariant_failed = true;
        }

        // A temporary binding must remain installed until every possibly
        // pressed key/modifier has been released and observed. Restoration is
        // a finally-stage and is still attempted when release proof failed.
        if temporary_mapping_restore_attempted {
            let mut restore_progress = KeyboardProgress::default();
            match self.restore_pending_keyboard_mapping(&mut restore_progress) {
                Ok(()) => temporary_mapping_restore_proven = true,
                Err(kind) => {
                    temporary_mapping_restore_failure = Some(kind);
                    proof_succeeded = false;
                }
            }
        }

        let mut service_recovered =
            proof_succeeded && !matches!(health_before, InputHealth::Poisoned(_));
        let health_transition = if invariant_failed {
            Ok(())
        } else if service_recovered {
            if matches!(health_before, InputHealth::ResetRequired(_)) {
                self.state.transition_health(HealthEvent::ResetSucceeded)
            } else {
                Ok(())
            }
        } else if matches!(self.state.health(), InputHealth::ResetRequired(_)) {
            self.state.transition_health(HealthEvent::ResetFailed)
        } else {
            Ok(())
        };
        if health_transition.is_err() {
            self.mark_panicked();
            invariant_failed = true;
            service_recovered = false;
            proof_succeeded = false;
        }
        if self.state.finish_action().is_err() {
            self.mark_panicked();
            let _abandoned_journal = self.state.finish_action();
            invariant_failed = true;
            service_recovered = false;
            proof_succeeded = false;
        }
        let report = self.cleanup_report(CleanupReportInputs {
            attempted,
            confirmed: if confirmed_batch { cleanup.len() } else { 0 },
            pointer: pointer_evidence,
            keys: key_evidence,
            unobservable_buttons,
            succeeded: proof_succeeded,
            temporary_mapping_restore_attempted,
            temporary_mapping_restore_proven,
        });
        if proof_succeeded {
            self.captured_keys.clear();
            self.synthesized_modifiers.clear();
        }
        if invariant_failed {
            Err(self.invariant_actor_failure(Some(report)))
        } else if let Some(kind) = temporary_mapping_restore_failure {
            Err(self.actor_failure(kind, Some(report)))
        } else if service_recovered {
            Ok(report)
        } else {
            Err(self.actor_failure(InputFailureKind::ResetFailed, Some(report)))
        }
    }

    fn run_keyboard_action(
        &mut self,
        action: &KeyboardAction,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<KeyboardProgress, KeyboardRunError> {
        let mut progress = KeyboardProgress {
            redact_scalar_evidence: action.contains_scalar_identifier(),
            ..KeyboardProgress::default()
        };
        match &action.kind {
            KeyboardActionKind::Down(identifier) => {
                let executed = match self.run_keyboard_down(
                    *identifier,
                    cancellation,
                    deadline,
                    &mut progress,
                ) {
                    Ok(executed) => executed,
                    Err(kind) => return Err(progress.failure(kind)),
                };
                if executed {
                    progress.completed_units = 1;
                    progress.stopped = deadline_stop(deadline);
                }
            }
            KeyboardActionKind::Up(identifier) => {
                let executed = match self.run_keyboard_up(
                    *identifier,
                    cancellation,
                    deadline,
                    &mut progress,
                ) {
                    Ok(executed) => executed,
                    Err(kind) => return Err(progress.failure(kind)),
                };
                if executed {
                    progress.completed_units = 1;
                    progress.stopped = deadline_stop(deadline);
                }
            }
            KeyboardActionKind::Sequence(steps) => {
                for (index, step) in steps.iter().enumerate() {
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        break;
                    }
                    let executed =
                        match self.run_keyboard_step(step, cancellation, deadline, &mut progress) {
                            Ok(executed) => executed,
                            Err(kind) => return Err(progress.failure(kind)),
                        };
                    if !executed {
                        break;
                    }
                    progress.completed_units = progress.completed_units.saturating_add(1);
                    if index + 1 < steps.len() {
                        if let Some(stopped) = boundary_stop(cancellation, deadline) {
                            progress.stopped = Some(stopped);
                            break;
                        }
                    } else {
                        progress.stopped = deadline_stop(deadline);
                    }
                }
            }
            KeyboardActionKind::Text {
                text,
                mode,
                inter_character_delay_ms,
            } => {
                progress.text_scalar_count = Some(text.chars().count());
                progress.requested_text_mode = Some(*mode);
                for (index, scalar) in text.chars().enumerate() {
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        break;
                    }
                    let delay_before_ms = if index == 0 {
                        0
                    } else {
                        *inter_character_delay_ms
                    };
                    match self.run_exact_scalar(
                        scalar,
                        delay_before_ms,
                        cancellation,
                        deadline,
                        &mut progress,
                    ) {
                        Ok(true) => {
                            progress.current_layout_scalars =
                                progress.current_layout_scalars.saturating_add(1);
                        }
                        Ok(false) => break,
                        Err(InputFailureKind::TextNotRepresentable)
                            if *mode == PhysicalTextMode::ExtendedTemporaryMapping =>
                        {
                            match self.run_temporary_scalar(
                                scalar,
                                delay_before_ms,
                                cancellation,
                                deadline,
                                &mut progress,
                            ) {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(kind) => return Err(progress.failure(kind)),
                            }
                        }
                        Err(kind) => return Err(progress.failure(kind)),
                    }
                    progress.completed_units = progress.completed_units.saturating_add(1);
                    if index + 1 < text.chars().count() {
                        if let Some(stopped) = boundary_stop(cancellation, deadline) {
                            progress.stopped = Some(stopped);
                            break;
                        }
                    } else {
                        progress.stopped = deadline_stop(deadline);
                    }
                }
            }
        }
        Ok(progress)
    }

    fn run_keyboard_step(
        &mut self,
        step: &KeyboardSequenceStep,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        let context = self.keyboard_resolution_context(KeyboardResolutionIntent::PhysicalKey)?;
        let bindings = self.resolve_and_validate_bindings(step.keys(), &context)?;
        if let Some(stopped) = boundary_stop(cancellation, deadline) {
            progress.stopped = Some(stopped);
            return Ok(false);
        }
        let generation = bindings
            .first()
            .map(|binding| binding.generation)
            .ok_or(InputFailureKind::StateRejected)?;
        let caller_keys: Vec<_> = bindings.iter().map(|binding| binding.key).collect();
        if has_duplicate_keys(&caller_keys)
            || caller_keys.iter().any(|key| {
                self.state
                    .pressed_keys()
                    .iter()
                    .any(|owned| owned.key() == *key)
            })
        {
            return Err(InputFailureKind::StateRejected);
        }

        let mut press_order = Vec::new();
        for modifier in bindings
            .iter()
            .flat_map(|binding| binding.required_modifiers.iter())
        {
            if !modifier.already_active
                && !caller_keys.contains(&modifier.key)
                && !self
                    .state
                    .pressed_keys()
                    .iter()
                    .any(|owned| owned.key() == modifier.key)
                && !press_order.iter().any(
                    |(key, _modifier): &(xenoteer_core::input::PhysicalKey, bool)| {
                        *key == modifier.key
                    },
                )
            {
                press_order.push((modifier.key, true));
            }
        }
        press_order.extend(
            bindings
                .iter()
                .filter(|binding| binding.is_modifier)
                .map(|binding| (binding.key, true)),
        );
        press_order.extend(
            bindings
                .iter()
                .filter(|binding| !binding.is_modifier)
                .map(|binding| (binding.key, false)),
        );
        if has_duplicate_keys(&press_order.iter().map(|(key, _)| *key).collect::<Vec<_>>()) {
            return Err(InputFailureKind::StateRejected);
        }

        if !progress.redact_scalar_evidence {
            progress
                .bindings
                .extend(bindings.iter().map(CapturedKeyBinding::evidence));
        }
        let mut events = Vec::with_capacity(press_order.len().saturating_mul(2));
        for (index, (key, modifier)) in press_order.iter().copied().enumerate() {
            events.push(key_planned_event(
                key,
                true,
                if index == 0 {
                    u32::from(step.delay_before_ms())
                } else {
                    0
                },
                modifier,
            ));
        }
        for (index, (key, modifier)) in press_order.iter().rev().copied().enumerate() {
            events.push(key_planned_event(
                key,
                false,
                if index == 0 {
                    u32::from(step.hold_ms())
                } else {
                    0
                },
                modifier,
            ));
        }
        let expected_up: Vec<_> = press_order.iter().map(|(key, _)| *key).collect();
        let post = self.run_key_batch(&events, generation, &expected_up, false, progress)?;
        self.validate_complete_bindings_after_effect(&bindings, &context, post)?;
        self.confirm_pending_key_batch()?;
        Ok(true)
    }

    fn run_exact_scalar(
        &mut self,
        scalar: char,
        delay_before_ms: u16,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        let step = KeyboardSequenceStep::press(KeyIdentifier::Scalar(scalar), 0, delay_before_ms)
            .map_err(|_| InputFailureKind::StateRejected)?;
        let context = self.keyboard_resolution_context(KeyboardResolutionIntent::ExactText)?;
        let bindings = self.resolve_and_validate_bindings(step.keys(), &context)?;
        self.run_resolved_complete_step(
            &step,
            bindings,
            ResolvedStepContext {
                resolution: &context,
                cancellation,
                deadline,
                record_binding_evidence: false,
            },
            progress,
        )
    }

    fn run_resolved_complete_step(
        &mut self,
        step: &KeyboardSequenceStep,
        bindings: Vec<CapturedKeyBinding>,
        execution: ResolvedStepContext<'_>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        if let Some(stopped) = boundary_stop(execution.cancellation, execution.deadline) {
            progress.stopped = Some(stopped);
            return Ok(false);
        }
        let generation = bindings
            .first()
            .map(|binding| binding.generation)
            .ok_or(InputFailureKind::StateRejected)?;
        let caller_keys: Vec<_> = bindings.iter().map(|binding| binding.key).collect();
        if has_duplicate_keys(&caller_keys)
            || caller_keys.iter().any(|key| {
                self.state
                    .pressed_keys()
                    .iter()
                    .any(|owned| owned.key() == *key)
            })
        {
            return Err(InputFailureKind::StateRejected);
        }
        let mut press_order = Vec::new();
        for RequiredModifierBinding {
            key,
            already_active,
        } in bindings
            .iter()
            .flat_map(|binding| binding.required_modifiers.iter().copied())
        {
            if !already_active
                && !caller_keys.contains(&key)
                && !self
                    .state
                    .pressed_keys()
                    .iter()
                    .any(|owned| owned.key() == key)
                && !press_order.iter().any(
                    |(existing, _): &(xenoteer_core::input::PhysicalKey, bool)| *existing == key,
                )
            {
                press_order.push((key, true));
            }
        }
        press_order.extend(
            bindings
                .iter()
                .filter(|binding| binding.is_modifier)
                .map(|binding| (binding.key, true)),
        );
        press_order.extend(
            bindings
                .iter()
                .filter(|binding| !binding.is_modifier)
                .map(|binding| (binding.key, false)),
        );
        if execution.record_binding_evidence && !progress.redact_scalar_evidence {
            progress
                .bindings
                .extend(bindings.iter().map(CapturedKeyBinding::evidence));
        }
        let mut events = Vec::with_capacity(press_order.len().saturating_mul(2));
        for (index, (key, modifier)) in press_order.iter().copied().enumerate() {
            events.push(key_planned_event(
                key,
                true,
                if index == 0 {
                    u32::from(step.delay_before_ms())
                } else {
                    0
                },
                modifier,
            ));
        }
        for (index, (key, modifier)) in press_order.iter().rev().copied().enumerate() {
            events.push(key_planned_event(
                key,
                false,
                if index == 0 {
                    u32::from(step.hold_ms())
                } else {
                    0
                },
                modifier,
            ));
        }
        let expected_up: Vec<_> = press_order.iter().map(|(key, _)| *key).collect();
        let post = self.run_key_batch(&events, generation, &expected_up, false, progress)?;
        self.validate_complete_bindings_after_effect(&bindings, execution.resolution, post)?;
        self.confirm_pending_key_batch()?;
        Ok(true)
    }

    fn run_keyboard_down(
        &mut self,
        identifier: KeyIdentifier,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        if self
            .captured_keys
            .iter()
            .any(|captured| captured.identifier == identifier)
        {
            return Err(InputFailureKind::StateRejected);
        }
        let context = self.keyboard_resolution_context(KeyboardResolutionIntent::PhysicalKey)?;
        let mut bindings = self.resolve_and_validate_bindings(&[identifier], &context)?;
        let binding = bindings.pop().ok_or(InputFailureKind::StateRejected)?;
        if let Some(stopped) = boundary_stop(cancellation, deadline) {
            progress.stopped = Some(stopped);
            return Ok(false);
        }
        if self
            .state
            .pressed_keys()
            .iter()
            .any(|owned| owned.key() == binding.key)
        {
            return Err(InputFailureKind::StateRejected);
        }
        if !progress.redact_scalar_evidence {
            progress.bindings.push(binding.evidence());
        }
        let mut synthesized = Vec::new();
        let mut newly_pressed = Vec::new();
        for modifier in &binding.required_modifiers {
            if synthesized.contains(&modifier.key) {
                continue;
            }
            if let Some(existing) = self
                .synthesized_modifiers
                .iter()
                .find(|existing| existing.key == modifier.key)
            {
                synthesized.push(existing.key);
            } else if !modifier.already_active
                && !self
                    .state
                    .pressed_keys()
                    .iter()
                    .any(|owned| owned.key() == modifier.key)
            {
                synthesized.push(modifier.key);
                newly_pressed.push(modifier.key);
            }
        }
        let mut events: Vec<_> = newly_pressed
            .iter()
            .copied()
            .map(|key| key_planned_event(key, true, 0, true))
            .collect();
        events.push(key_planned_event(binding.key, true, 0, binding.is_modifier));
        let generation = binding.generation;
        let post = self.run_key_batch(&events, generation, &[], true, progress)?;
        let post_context =
            self.keyboard_resolution_context(KeyboardResolutionIntent::PhysicalKey)?;
        let post_binding = match self
            .keyboard
            .resolve_synchronized(identifier, &post_context)
        {
            Ok(binding) => binding,
            Err(fault) => {
                self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                return Err(self.keyboard_identifier_failure(fault, identifier, true));
            }
        };
        let equivalent = if post.accepted_initial_xtest_set_map {
            post_binding.generation == post.model.generation
                && binding.physically_equivalent_across_generation(&post_binding)
        } else {
            binding.physically_equivalent(&post_binding)
        };
        if !equivalent {
            self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
            return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
        }
        let held_binding = if post.accepted_initial_xtest_set_map {
            post_binding
        } else {
            binding
        };
        self.confirm_pending_key_batch()?;
        for key in &synthesized {
            if let Some(existing) = self
                .synthesized_modifiers
                .iter_mut()
                .find(|existing| existing.key == *key)
            {
                existing.references = existing.references.saturating_add(1);
            } else {
                self.synthesized_modifiers.push(SynthesizedModifierRef {
                    key: *key,
                    references: 1,
                });
            }
        }
        self.captured_keys.push(CapturedKeyHold {
            identifier,
            binding: held_binding,
            synthesized_modifiers: synthesized,
        });
        Ok(true)
    }

    fn run_keyboard_up(
        &mut self,
        identifier: KeyIdentifier,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        let position = self
            .captured_keys
            .iter()
            .position(|captured| captured.identifier == identifier)
            .ok_or(InputFailureKind::StateRejected)?;
        let hold = self.captured_keys.remove(position);
        let context = self.keyboard_resolution_context(KeyboardResolutionIntent::PhysicalKey)?;
        let (held_generation, deferred_validation_failure) = match self
            .keyboard
            .validate_held_binding_synchronized(&hold.binding, identifier, &context)
        {
            Ok(generation) => (generation, None),
            Err(fault) => {
                if fault.kind.is_connection() {
                    self.captured_keys.insert(position, hold);
                    return Err(self.keyboard_identifier_failure(fault, identifier, false));
                }
                let deferred = self.keyboard_identifier_failure(fault, identifier, true);
                // Binding validation can fail because the model or observed
                // pressed state changed. The actor still owns the privately
                // captured keycode and must attempt that exact release instead
                // of leaving a healthy but permanently wedged held identity.
                (
                    HeldBindingGeneration::Stale {
                        captured: hold.binding.generation,
                        current: self
                            .keyboard
                            .diagnostics()
                            .generation
                            .unwrap_or(hold.binding.generation),
                    },
                    Some(deferred),
                )
            }
        };
        if let Some(stopped) = boundary_stop(cancellation, deadline) {
            self.captured_keys.insert(position, hold);
            progress.stopped = Some(stopped);
            return Ok(false);
        }
        if !progress.redact_scalar_evidence {
            progress.bindings.push(hold.binding.evidence());
        }
        let mut release_modifiers = Vec::new();
        for key in hold.synthesized_modifiers.iter().rev() {
            if self
                .synthesized_modifiers
                .iter()
                .find(|existing| existing.key == *key)
                .is_some_and(|existing| existing.references == 1)
            {
                release_modifiers.push(*key);
            }
        }
        let mut events = vec![key_planned_event(
            hold.binding.key,
            false,
            0,
            hold.binding.is_modifier,
        )];
        events.extend(
            release_modifiers
                .iter()
                .copied()
                .map(|key| key_planned_event(key, false, 0, true)),
        );
        let expected_up: Vec<_> = events
            .iter()
            .filter_map(|event| match event.backend {
                BackendEvent::Key {
                    key,
                    pressed: false,
                    ..
                } => Some(key),
                _ => None,
            })
            .collect();
        let post = match self.run_key_batch(
            &events,
            hold.binding.generation,
            &expected_up,
            false,
            progress,
        ) {
            Ok(post) => post,
            Err(kind) => {
                self.captured_keys.insert(position, hold);
                return Err(kind);
            }
        };
        self.confirm_pending_key_batch()?;
        for key in &hold.synthesized_modifiers {
            if let Some(existing) = self
                .synthesized_modifiers
                .iter_mut()
                .find(|existing| existing.key == *key)
            {
                existing.references = existing.references.saturating_sub(1);
            }
        }
        self.synthesized_modifiers
            .retain(|existing| existing.references > 0);
        let mapping_changed = matches!(held_generation, HeldBindingGeneration::Stale { .. })
            || post.model.generation != hold.binding.generation;
        if let Some(kind) = deferred_validation_failure {
            return Err(kind);
        }
        if mapping_changed {
            return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
        }
        Ok(true)
    }

    fn keyboard_resolution_context(
        &mut self,
        intent: KeyboardResolutionIntent,
    ) -> Result<KeyboardResolutionContext, InputFailureKind> {
        let owned: Vec<_> = self
            .state
            .pressed_keys()
            .iter()
            .map(|owned| owned.key().keycode())
            .collect();
        KeyboardResolutionContext::new(intent, &owned).map_err(|_| {
            self.mark_panicked();
            InputFailureKind::ActorPanicked
        })
    }

    fn resolve_and_validate_bindings(
        &mut self,
        identifiers: &[KeyIdentifier],
        context: &KeyboardResolutionContext,
    ) -> Result<Vec<CapturedKeyBinding>, InputFailureKind> {
        let mut bindings = Vec::with_capacity(identifiers.len());
        let mut generation = None;
        for identifier in identifiers {
            let binding = self
                .keyboard
                .resolve_synchronized(*identifier, context)
                .map_err(|fault| self.keyboard_identifier_failure(fault, *identifier, false))?;
            if generation.is_some_and(|generation| generation != binding.generation) {
                return Err(InputFailureKind::KeyboardMappingChangedBeforeEffect);
            }
            generation = Some(binding.generation);
            bindings.push(binding);
        }
        for binding in &bindings {
            let preflight = self
                .keyboard
                .validate_binding_synchronized(binding, context)
                .map_err(|fault| {
                    self.keyboard_identifier_failure(fault, binding.identifier, false)
                })?;
            if preflight.generation != binding.generation {
                return Err(InputFailureKind::KeyboardMappingChangedBeforeEffect);
            }
        }
        Ok(bindings)
    }

    fn run_key_batch(
        &mut self,
        events: &[PlannedEvent],
        expected_generation: u64,
        expected_up: &[xenoteer_core::input::PhysicalKey],
        enforce_generation: bool,
        progress: &mut KeyboardProgress,
    ) -> Result<KeyBatchPostflight, InputFailureKind> {
        let first_keyboard_effect = !self.keyboard_effect_emitted;
        let pre_fingerprint = self.keyboard.diagnostics().keymap_fingerprint;
        let batch = match self.run_batch(events, false, false) {
            Ok(batch) => batch,
            Err(failure) => {
                if failure.sent > 0 {
                    self.keyboard_effect_emitted = true;
                }
                let kind = failure.public_kind();
                progress.events_emitted = progress.events_emitted.saturating_add(failure.sent);
                if let Some(observation) = failure.observation {
                    progress.pointer = Some(observation);
                }
                return Err(kind);
            }
        };
        if batch.sent > 0 {
            self.keyboard_effect_emitted = true;
        }
        progress.events_emitted = progress.events_emitted.saturating_add(batch.sent);
        progress.pointer = Some(batch.observation);
        let pressed = match self.backend.observe_keys() {
            Ok(pressed) => pressed,
            Err(fault) => {
                let kind = if fault.kind == BackendFaultKind::Connection {
                    self.apply_backend_fault(&fault);
                    InputFailureKind::BackendUnavailable
                } else {
                    self.fail_pending_key_batch(ResetReason::BarrierFailed)?;
                    InputFailureKind::BarrierFailed
                };
                return Err(kind);
            }
        };
        self.sent_press_ledger.reconcile_keys(&pressed);
        let preflight = match self.keyboard.synchronize_preflight() {
            Ok(preflight) => preflight,
            Err(fault) => {
                let kind = self.keyboard_model_failure(fault, true);
                if self.state.health() == InputHealth::Healthy {
                    self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                }
                return Err(kind);
            }
        };
        let accepted_initial_xtest_set_map = first_keyboard_effect
            && batch.sent > 0
            && preflight.generation == expected_generation.saturating_add(1)
            && preflight.mapping_invalidations == 1
            && preflight.structural_set_map_invalidations == 1
            && pre_fingerprint.is_some()
            && preflight.keymap_fingerprint == pre_fingerprint;
        if enforce_generation
            && preflight.generation != expected_generation
            && !accepted_initial_xtest_set_map
        {
            self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
            return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
        }
        let expected_down_valid = self
            .state
            .pressed_keys()
            .iter()
            .all(|owned| expected_up.contains(&owned.key()) || pressed.contains(&owned.key()));
        let expected_up_valid = expected_up.iter().all(|key| !pressed.contains(key));
        if !expected_down_valid || !expected_up_valid {
            self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
            return Err(InputFailureKind::PostconditionFailed);
        }
        Ok(KeyBatchPostflight {
            model: preflight,
            accepted_initial_xtest_set_map,
        })
    }

    fn validate_complete_bindings_after_effect(
        &mut self,
        bindings: &[CapturedKeyBinding],
        context: &KeyboardResolutionContext,
        postflight: KeyBatchPostflight,
    ) -> Result<(), InputFailureKind> {
        let captured_generation = bindings
            .first()
            .map(|binding| binding.generation)
            .ok_or(InputFailureKind::StateRejected)?;
        if postflight.model.generation != captured_generation {
            if !postflight.accepted_initial_xtest_set_map {
                self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
            }
            for binding in bindings {
                let current = match self
                    .keyboard
                    .resolve_synchronized(binding.identifier, context)
                {
                    Ok(current) => current,
                    Err(fault) => {
                        self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                        return Err(self.keyboard_identifier_failure(
                            fault,
                            binding.identifier,
                            true,
                        ));
                    }
                };
                if current.generation != postflight.model.generation
                    || !binding.physically_equivalent_across_generation(&current)
                {
                    self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                    return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
                }
            }
            let final_preflight = match self.keyboard.synchronize_preflight() {
                Ok(preflight) => preflight,
                Err(fault) => {
                    self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                    return Err(self.keyboard_model_failure(fault, true));
                }
            };
            if final_preflight.generation != postflight.model.generation
                || final_preflight.mapping_invalidations != 0
                || final_preflight.keymap_fingerprint != postflight.model.keymap_fingerprint
            {
                self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
            }
            return Ok(());
        }
        for binding in bindings {
            let preflight = match self
                .keyboard
                .validate_binding_synchronized(binding, context)
            {
                Ok(preflight) => preflight,
                Err(fault) => {
                    self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                    return Err(self.keyboard_identifier_failure(fault, binding.identifier, true));
                }
            };
            if preflight.generation != binding.generation {
                self.fail_pending_key_batch(ResetReason::PostconditionFailed)?;
                return Err(InputFailureKind::KeyboardMappingChangedAfterEffect);
            }
        }
        Ok(())
    }

    fn confirm_pending_key_batch(&mut self) -> Result<(), InputFailureKind> {
        if self.state.confirm_batch().is_err() {
            self.mark_panicked();
            Err(InputFailureKind::ActorPanicked)
        } else {
            Ok(())
        }
    }

    fn fail_pending_key_batch(&mut self, reason: ResetReason) -> Result<(), InputFailureKind> {
        if self.state.fail_batch(reason).is_err() {
            self.mark_panicked();
            Err(InputFailureKind::ActorPanicked)
        } else {
            Ok(())
        }
    }

    fn keyboard_model_failure(
        &mut self,
        fault: KeyboardModelFault,
        _after_effect: bool,
    ) -> InputFailureKind {
        match fault.kind {
            KeyboardModelFaultKind::Unavailable => InputFailureKind::UnsupportedByBackend,
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::Conflict if _after_effect => {
                InputFailureKind::ModifierConflictAfterEffect
            }
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::Conflict => InputFailureKind::ModifierConflict,
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::NotRepresentable => InputFailureKind::TextNotRepresentable,
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::MappingChanged => {
                if _after_effect {
                    InputFailureKind::KeyboardMappingChangedAfterEffect
                } else {
                    InputFailureKind::KeyboardMappingChangedBeforeEffect
                }
            }
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::Unsafe | KeyboardModelFaultKind::Platform => {
                InputFailureKind::UnsupportedByBackend
            }
            #[cfg(feature = "native-xkbcommon")]
            KeyboardModelFaultKind::Connection => {
                let _ignored = self.state.transition_health(HealthEvent::ConnectionLost);
                InputFailureKind::BackendUnavailable
            }
        }
    }

    fn keyboard_identifier_failure(
        &mut self,
        fault: KeyboardModelFault,
        identifier: KeyIdentifier,
        after_effect: bool,
    ) -> InputFailureKind {
        if fault.kind.is_not_representable() && !matches!(identifier, KeyIdentifier::Scalar(_)) {
            InputFailureKind::UnsupportedByBackend
        } else {
            self.keyboard_model_failure(fault, after_effect)
        }
    }

    fn run_temporary_scalar(
        &mut self,
        scalar: char,
        delay_before_ms: u16,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        progress: &mut KeyboardProgress,
    ) -> Result<bool, InputFailureKind> {
        if let Some(stopped) = boundary_stop(cancellation, deadline) {
            progress.stopped = Some(stopped);
            return Ok(false);
        }
        let context = self.keyboard_resolution_context(KeyboardResolutionIntent::ExactText)?;
        let reservation = self.keyboard.reserve_unused_keycode().map_err(|fault| {
            if fault.kind.is_not_representable() {
                InputFailureKind::TemporaryMappingInstallFailed
            } else {
                self.keyboard_model_failure(fault, false)
            }
        })?;
        self.keyboard
            .validate_reservation(&reservation)
            .map_err(|fault| self.keyboard_model_failure(fault, false))?;
        if self
            .state
            .pressed_keys()
            .iter()
            .any(|owned| owned.key() == reservation.key)
        {
            return Err(InputFailureKind::StateRejected);
        }
        let original = match self.backend.read_keyboard_mapping(reservation.key) {
            Ok(mapping) => mapping,
            Err(fault) => {
                if fault.kind == BackendFaultKind::Connection {
                    self.apply_backend_fault(&fault);
                }
                return Err(backend_public_kind(&fault));
            }
        };
        if original.keysyms_per_keycode == 0
            || original.keysyms.len() != usize::from(original.keysyms_per_keycode)
            || original.keysyms.iter().any(|keysym| *keysym != 0)
        {
            return Err(InputFailureKind::UnsupportedByBackend);
        }
        self.keyboard
            .validate_reservation(&reservation)
            .map_err(|fault| self.keyboard_model_failure(fault, false))?;
        if let Some(stopped) = boundary_stop(cancellation, deadline) {
            progress.stopped = Some(stopped);
            return Ok(false);
        }

        let mut installed = CoreKeyboardMapping {
            keysyms_per_keycode: original.keysyms_per_keycode,
            keysyms: vec![0; original.keysyms.len()],
        };
        installed.keysyms[0] = unicode_keysym(scalar);
        // Persist exact restoration state before the first mutating request.
        self.pending_keyboard_restore = Some(PendingTemporaryRestore {
            key: reservation.key,
            original: original.clone(),
        });
        progress.temporary_mapping_restoration_proven = Some(false);

        let install_result = self
            .backend
            .write_keyboard_mapping(reservation.key, &installed)
            .and_then(|()| self.backend.read_keyboard_mapping(reservation.key))
            .and_then(|observed| {
                let target = installed.keysyms[0];
                // XKB's core-keymap projection may mirror an unshifted symbol
                // into slot 2 while retaining the server's wider row. Accept
                // only that canonical duplicate; any other populated slot is
                // an external mutation or a semantically different mapping.
                if observed.keysyms_per_keycode == installed.keysyms_per_keycode
                    && observed.keysyms.len() == installed.keysyms.len()
                    && observed.keysyms.first() == Some(&target)
                    && observed
                        .keysyms
                        .iter()
                        .enumerate()
                        .skip(1)
                        .all(|(index, keysym)| *keysym == 0 || (index == 2 && *keysym == target))
                {
                    Ok(())
                } else {
                    Err(BackendFault::new(
                        BackendFaultKind::Request,
                        "temporary keyboard mapping readback differed",
                    ))
                }
            });
        if let Err(fault) = install_result {
            let connection_lost = fault.kind == BackendFaultKind::Connection;
            if connection_lost {
                self.apply_backend_fault(&fault);
            }
            let restore = self.restore_pending_keyboard_mapping(progress);
            if connection_lost {
                return Err(InputFailureKind::BackendUnavailable);
            }
            return match restore {
                Ok(()) => Err(InputFailureKind::TemporaryMappingInstallFailed),
                Err(kind) => Err(kind),
            };
        }
        progress.temporary_mappings_installed =
            progress.temporary_mappings_installed.saturating_add(1);

        let prepared = (|| {
            self.keyboard
                .synchronize_preflight()
                .map_err(|fault| self.keyboard_model_failure(fault, false))?;
            let binding = self
                .keyboard
                .resolve_synchronized(KeyIdentifier::Scalar(scalar), &context)
                .map_err(|fault| self.temporary_mapping_preflight_failure(fault))?;
            if binding.key != reservation.key {
                return Err(InputFailureKind::TemporaryMappingInstallFailed);
            }
            let validation = self
                .keyboard
                .validate_binding_synchronized(&binding, &context)
                .map_err(|fault| self.temporary_mapping_preflight_failure(fault))?;
            if validation.generation != binding.generation {
                return Err(InputFailureKind::TemporaryMappingInstallFailed);
            }
            let step =
                KeyboardSequenceStep::press(KeyIdentifier::Scalar(scalar), 0, delay_before_ms)
                    .map_err(|_| InputFailureKind::StateRejected)?;
            self.run_resolved_complete_step(
                &step,
                vec![binding],
                ResolvedStepContext {
                    resolution: &context,
                    cancellation,
                    deadline,
                    record_binding_evidence: false,
                },
                progress,
            )
        })();

        if prepared.is_err() && self.state.health() != InputHealth::Healthy {
            return prepared;
        }
        let restore = self.restore_pending_keyboard_mapping(progress);
        match (prepared, restore) {
            (_, Err(kind)) => Err(kind),
            (Err(kind), Ok(())) => Err(kind),
            (Ok(false), Ok(())) => Ok(false),
            (Ok(true), Ok(())) => {
                progress.temporary_mapping_scalars =
                    progress.temporary_mapping_scalars.saturating_add(1);
                Ok(true)
            }
        }
    }

    fn restore_pending_keyboard_mapping(
        &mut self,
        progress: &mut KeyboardProgress,
    ) -> Result<(), InputFailureKind> {
        let Some(pending) = self.pending_keyboard_restore.clone() else {
            return Ok(());
        };
        let backend_restore = self
            .backend
            .write_keyboard_mapping(pending.key, &pending.original)
            .and_then(|()| self.backend.read_keyboard_mapping(pending.key))
            .and_then(|observed| {
                if observed == pending.original {
                    Ok(())
                } else {
                    Err(BackendFault::new(
                        BackendFaultKind::Request,
                        "temporary keyboard mapping restoration readback differed",
                    ))
                }
            });
        if let Err(fault) = backend_restore {
            progress.temporary_mapping_restoration_proven = Some(false);
            if fault.kind == BackendFaultKind::Connection {
                self.apply_backend_fault(&fault);
                return Err(InputFailureKind::BackendUnavailable);
            }
            let _ignored = self
                .state
                .transition_health(HealthEvent::TemporaryKeyboardMappingRestoreFailed);
            return Err(InputFailureKind::TemporaryMappingRestoreFailed);
        }
        if let Err(fault) = self.keyboard.synchronize_preflight() {
            progress.temporary_mapping_restoration_proven = Some(false);
            if fault.kind.is_connection() {
                let _ignored = self.state.transition_health(HealthEvent::ConnectionLost);
                return Err(InputFailureKind::BackendUnavailable);
            }
            let _ignored = self
                .state
                .transition_health(HealthEvent::TemporaryKeyboardMappingRestoreFailed);
            return Err(InputFailureKind::TemporaryMappingRestoreFailed);
        }
        {
            self.pending_keyboard_restore = None;
            progress.temporary_mappings_restored =
                progress.temporary_mappings_restored.saturating_add(1);
            progress.temporary_mapping_restoration_proven = Some(true);
            Ok(())
        }
    }

    fn temporary_mapping_preflight_failure(
        &mut self,
        fault: KeyboardModelFault,
    ) -> InputFailureKind {
        match fault.kind {
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::NotRepresentable
            | KeyboardModelFaultKind::Unsafe
            | KeyboardModelFaultKind::Platform
            | KeyboardModelFaultKind::Unavailable => {
                InputFailureKind::TemporaryMappingInstallFailed
            }
            #[cfg(not(any(test, feature = "native-xkbcommon")))]
            KeyboardModelFaultKind::Unavailable => InputFailureKind::TemporaryMappingInstallFailed,
            #[cfg(any(test, feature = "native-xkbcommon"))]
            KeyboardModelFaultKind::Conflict | KeyboardModelFaultKind::MappingChanged => {
                self.keyboard_model_failure(fault, false)
            }
            #[cfg(feature = "native-xkbcommon")]
            KeyboardModelFaultKind::Connection => self.keyboard_model_failure(fault, false),
        }
    }

    fn conservative_cleanup_actions(&self) -> Vec<CleanupAction> {
        let owned = plan_cleanup(&self.state);
        let mut actions = Vec::with_capacity(owned.len().saturating_add(512));
        actions.extend(
            owned
                .actions()
                .iter()
                .copied()
                .filter(|action| matches!(action, CleanupAction::ReleaseButton { .. })),
        );
        for detail in (1_u8..=u8::MAX).rev() {
            if !bit_is_set(&self.sent_press_ledger.buttons, detail) {
                continue;
            }
            let Ok(button) = PhysicalButton::new(detail) else {
                continue;
            };
            if !actions.iter().any(|action| {
                matches!(action, CleanupAction::ReleaseButton { button: existing } if *existing == button)
            }) {
                actions.push(CleanupAction::ReleaseButton { button });
            }
        }
        for modifier in [false, true] {
            actions.extend(owned.actions().iter().copied().filter(|action| {
                matches!(action, CleanupAction::ReleaseKey { modifier: value, .. } if *value == modifier)
            }));
            for detail in (8_u8..=u8::MAX).rev() {
                if !bit_is_set(&self.sent_press_ledger.keys, detail)
                    || bit_is_set(&self.sent_press_ledger.modifier_keys, detail) != modifier
                {
                    continue;
                }
                let Ok(key) = xenoteer_core::input::PhysicalKey::new(detail) else {
                    continue;
                };
                if !actions.iter().any(|action| {
                    matches!(action, CleanupAction::ReleaseKey { key: existing, .. } if *existing == key)
                }) {
                    actions.push(CleanupAction::ReleaseKey { key, modifier });
                }
            }
        }
        actions
    }

    pub(super) fn emergency_cleanup_after_panic(&mut self) -> bool {
        let buttons_released =
            catch_unwind(AssertUnwindSafe(|| self.emergency_release_buttons())).unwrap_or(false);
        let nonmodifiers_released =
            catch_unwind(AssertUnwindSafe(|| self.emergency_release_keys(false))).unwrap_or(false);
        let modifiers_released =
            catch_unwind(AssertUnwindSafe(|| self.emergency_release_keys(true))).unwrap_or(false);
        let mapping_restored = catch_unwind(AssertUnwindSafe(|| {
            self.pending_keyboard_restore.is_none()
                || self
                    .restore_pending_keyboard_mapping(&mut KeyboardProgress::default())
                    .is_ok()
        }))
        .unwrap_or(false);
        buttons_released && nonmodifiers_released && modifiers_released && mapping_restored
    }

    fn emergency_release_keys(&mut self, modifier: bool) -> bool {
        let owned_keys: Vec<_> = self
            .conservative_cleanup_actions()
            .into_iter()
            .filter_map(|action| match action {
                CleanupAction::ReleaseKey {
                    key,
                    modifier: value,
                } if value == modifier => Some(key),
                CleanupAction::ReleaseButton { .. } | CleanupAction::ReleaseKey { .. } => None,
            })
            .collect();
        let mut cookies = Vec::with_capacity(owned_keys.len());
        let mut requests_ok = true;
        for key in &owned_keys {
            match self.backend.send_event(BackendEvent::Key {
                key: *key,
                pressed: false,
                delay_ms: 0,
            }) {
                Ok(cookie) => cookies.push(cookie),
                Err(_) => requests_ok = false,
            }
        }
        for cookie in cookies {
            if B::check_cookie(cookie).is_err() {
                requests_ok = false;
            }
        }
        let keys_ok = self.backend.observe_keys().is_ok_and(|pressed| {
            self.sent_press_ledger.reconcile_keys(&pressed);
            owned_keys.iter().all(|key| !pressed.contains(key))
        });
        requests_ok && keys_ok
    }

    fn emergency_release_buttons(&mut self) -> bool {
        let owned_buttons: Vec<_> = self
            .conservative_cleanup_actions()
            .into_iter()
            .filter_map(|action| match action {
                CleanupAction::ReleaseButton { button } => Some(button),
                CleanupAction::ReleaseKey { .. } => None,
            })
            .collect();
        let mut cookies = Vec::with_capacity(owned_buttons.len());
        let mut requests_ok = true;
        for button in &owned_buttons {
            match self.backend.send_event(BackendEvent::Button {
                button: *button,
                pressed: false,
                delay_ms: 0,
            }) {
                Ok(cookie) => cookies.push(cookie),
                Err(_) => requests_ok = false,
            }
        }
        for cookie in cookies {
            if B::check_cookie(cookie).is_err() {
                requests_ok = false;
            }
        }
        let pointer_ok = self.backend.observe_pointer().is_ok_and(|observation| {
            self.sent_press_ledger
                .reconcile_buttons(&self.button_mapping, observation.logical_buttons_1_to_5);
            owned_buttons.iter().all(|button| {
                logical_for_physical(&self.button_mapping, *button).is_some_and(|logical| {
                    (1..=5).contains(&logical)
                        && !observation.logical_buttons_1_to_5[usize::from(logical - 1)]
                })
            })
        });
        requests_ok && pointer_ok
    }

    fn keyboard_outcome_evidence(&self, progress: KeyboardProgress) -> KeyboardOutcomeEvidence {
        KeyboardOutcomeEvidence {
            model: self.keyboard_diagnostics(progress.redact_scalar_evidence),
            bindings: progress.bindings,
            text_scalar_count: progress.text_scalar_count,
            requested_text_mode: progress.requested_text_mode,
            current_layout_scalars: progress.current_layout_scalars,
            temporary_mapping_scalars: progress.temporary_mapping_scalars,
            temporary_mappings_installed: progress.temporary_mappings_installed,
            temporary_mappings_restored: progress.temporary_mappings_restored,
            temporary_mapping_restoration_proven: progress.temporary_mapping_restoration_proven,
        }
    }

    fn keyboard_run_failure(
        &self,
        context: ActionContext,
        error: KeyboardRunError,
        effects: Option<EffectJournal>,
        cleanup: Option<Box<InputCleanupEvidence>>,
    ) -> InputFailure {
        let pointer = error.progress.pointer.clone();
        let keyboard = KeyboardOutcomeEvidence {
            model: self.keyboard_diagnostics(error.progress.redact_scalar_evidence),
            bindings: error.progress.bindings,
            text_scalar_count: error.progress.text_scalar_count,
            requested_text_mode: error.progress.requested_text_mode,
            current_layout_scalars: error.progress.current_layout_scalars,
            temporary_mapping_scalars: error.progress.temporary_mapping_scalars,
            temporary_mappings_installed: error.progress.temporary_mappings_installed,
            temporary_mappings_restored: error.progress.temporary_mappings_restored,
            temporary_mapping_restoration_proven: error
                .progress
                .temporary_mapping_restoration_proven,
        };
        InputFailure {
            command_id: Some(context.command_id),
            kind: error.kind,
            events_emitted: error.progress.events_emitted,
            completed_units: error.progress.completed_units,
            progress_known: error.progress_known,
            requested_pointer: None,
            last_observed_pointer: pointer.as_ref().map(|value| value.pointer),
            observed_logical_buttons_1_to_5: pointer.map(|value| value.logical_buttons_1_to_5),
            button_observation_partial: false,
            effects: effects.map(|journal| {
                Box::new(InputEffectEvidence::from_journal(
                    journal,
                    error.progress.redact_scalar_evidence,
                ))
            }),
            cleanup: cleanup.map(|cleanup| {
                if error.progress.redact_scalar_evidence {
                    Box::new((*cleanup).redact_keyboard())
                } else {
                    cleanup
                }
            }),
            keyboard: Some(Box::new(keyboard)),
        }
    }

    fn run_action(
        &mut self,
        action: &InputAction,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
        mut window_click: Option<&mut WindowClickRevalidation>,
    ) -> Result<ActionProgress, ActionError> {
        match action {
            InputAction::Move(action) => {
                let mut progress = ActionProgress {
                    requested_pointer: Some(action.plan().end()),
                    ..ActionProgress::default()
                };
                self.require_observed_start(action.plan(), &mut progress)?;
                if let Some(stopped) = boundary_stop(cancellation, deadline) {
                    progress.stopped = Some(stopped);
                    return Ok(progress);
                }
                if action.plan().event_count() == 0 {
                    match self.backend.observe_pointer() {
                        Ok(observation) => {
                            self.last_pointer = Some(observation.pointer);
                            progress.observed_pointer = Some(observation.pointer);
                            progress.observed_buttons = Some(observation.logical_buttons_1_to_5);
                            progress.completed_units = 1;
                            return Ok(progress);
                        }
                        Err(fault) => {
                            let kind = if fault.kind == BackendFaultKind::Connection {
                                self.apply_backend_fault(&fault);
                                InputFailureKind::BackendUnavailable
                            } else {
                                let _ignored = self.state.transition_health(
                                    HealthEvent::RequireReset(ResetReason::BarrierFailed),
                                );
                                InputFailureKind::BarrierFailed
                            };
                            return Err(progress.error(kind));
                        }
                    }
                }
                let segment_count = action.plan().segment_count();
                for (index, segment) in action.plan().segments().enumerate() {
                    if index > 0
                        && let Some(stopped) = boundary_stop(cancellation, deadline)
                    {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    let batch = self
                        .run_batch(&motion_sample_events(segment), false, true)
                        .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                    absorb_batch_success(&mut progress, &batch);
                    if deadline_elapsed(deadline) {
                        if index.saturating_add(1) == segment_count {
                            progress.completed_units = 1;
                        }
                        progress.stopped = Some(BoundaryStop::Deadline);
                        return Ok(progress);
                    }
                }
                progress.completed_units = 1;
                Ok(progress)
            }
            InputAction::Click(action) => {
                let mut progress = ActionProgress {
                    requested_pointer: action.movement().map(|plan| plan.end()),
                    ..ActionProgress::default()
                };
                if let Some(movement) = action.movement() {
                    self.require_observed_start(movement, &mut progress)?;
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    for (index, segment) in movement.segments().enumerate() {
                        if index > 0
                            && let Some(stopped) = boundary_stop(cancellation, deadline)
                        {
                            progress.stopped = Some(stopped);
                            return Ok(progress);
                        }
                        let batch = self
                            .run_batch(&motion_sample_events(segment), false, true)
                            .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                        absorb_batch_success(&mut progress, &batch);
                        if deadline_elapsed(deadline) {
                            progress.stopped = Some(BoundaryStop::Deadline);
                            return Ok(progress);
                        }
                    }
                    if progress.observed_pointer != Some(movement.end()) {
                        return Err(progress.error(InputFailureKind::PostconditionFailed));
                    }
                }
                for click_index in 0..action.count() {
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    self.drain_for_action(&progress)?;
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    if window_click.is_some() {
                        let dwell_ms = if click_index == 0 {
                            action.pre_click_dwell_ms()
                        } else {
                            action.inter_click_interval_ms()
                        };
                        self.run_window_click_dwell(dwell_ms);
                        self.drain_for_action(&progress)?;
                        if let Some(stopped) = boundary_stop(cancellation, deadline) {
                            progress.stopped = Some(stopped);
                            return Ok(progress);
                        }
                    }
                    let button = self
                        .resolve_button(action.logical_button())
                        .map_err(|kind| progress.error(kind))?;
                    progress.button_observation_partial = action.logical_button().number() > 5;
                    if self.state.pressed_buttons().contains(&button) {
                        return Err(progress.error(InputFailureKind::StateRejected));
                    }
                    if let Some(staged) = window_click.as_deref_mut() {
                        self.revalidate_window_click(staged, &mut progress)?;
                    }
                    let press_delay = if window_click.is_some() {
                        0
                    } else if click_index == 0 {
                        u32::from(action.pre_click_dwell_ms())
                    } else {
                        u32::from(action.inter_click_interval_ms())
                    };
                    let planned =
                        button_pair(button, press_delay, u32::from(action.press_duration_ms()));
                    let batch = self
                        .run_batch(&planned, true, false)
                        .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                    progress.events_emitted += batch.sent;
                    progress.observed_pointer = Some(batch.observation.pointer);
                    progress.observed_buttons = Some(batch.observation.logical_buttons_1_to_5);
                    if !logical_button_released(
                        action.logical_button(),
                        batch.observation.logical_buttons_1_to_5,
                    ) {
                        return Err(self.fail_observed_batch(&progress));
                    }
                    self.confirm_observed_batch(&progress)?;
                    if action
                        .movement()
                        .is_some_and(|movement| batch.observation.pointer != movement.end())
                    {
                        return Err(progress.error(InputFailureKind::PostconditionFailed));
                    }
                    progress.button_observation_partial = action.logical_button().number() > 5;
                    progress.completed_units = progress.completed_units.saturating_add(1);
                    if deadline_elapsed(deadline) {
                        progress.stopped = Some(BoundaryStop::Deadline);
                        return Ok(progress);
                    }
                }
                Ok(progress)
            }
            InputAction::Drag(action) => {
                let mut progress = ActionProgress {
                    requested_pointer: Some(action.movement().end()),
                    ..ActionProgress::default()
                };
                self.require_observed_start(action.movement(), &mut progress)?;
                self.drain_for_action(&progress)?;
                if let Some(stopped) = boundary_stop(cancellation, deadline) {
                    progress.stopped = Some(stopped);
                    return Ok(progress);
                }
                let button = self
                    .resolve_button(action.logical_button())
                    .map_err(|kind| progress.error(kind))?;
                progress.button_observation_partial = action.logical_button().number() > 5;
                if self.state.pressed_buttons().contains(&button) {
                    return Err(progress.error(InputFailureKind::StateRejected));
                }
                let mut press_and_dwell = Vec::with_capacity(2);
                press_and_dwell.push(button_event(button, true, 0, false));
                let start = action.movement().start();
                press_and_dwell.push(PlannedEvent {
                    backend: BackendEvent::Motion {
                        point: start,
                        delay_ms: u32::from(action.press_dwell_ms()),
                    },
                    effect: StateEffect::Motion(start),
                });
                let batch = self
                    .run_batch(&press_and_dwell, true, false)
                    .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                absorb_batch_success(&mut progress, &batch);
                if !logical_button_pressed(
                    action.logical_button(),
                    batch.observation.logical_buttons_1_to_5,
                ) {
                    return Err(self.fail_observed_batch(&progress));
                }
                self.confirm_observed_batch(&progress)?;
                progress.stopped = boundary_stop(cancellation, deadline);
                let mut traversed_segments = 0;
                if progress.stopped.is_none() {
                    for segment in action.movement().segments() {
                        if let Some(stopped) = boundary_stop(cancellation, deadline) {
                            progress.stopped = Some(stopped);
                            break;
                        }
                        let batch = self
                            .run_batch(&motion_sample_events(segment), true, true)
                            .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                        absorb_batch_success(&mut progress, &batch);
                        traversed_segments += 1;
                        if let Some(stopped) = boundary_stop(cancellation, deadline) {
                            progress.stopped = Some(stopped);
                            break;
                        }
                    }
                }
                let traversed_all_segments =
                    traversed_segments == action.movement().segment_count();
                let release = [button_event(
                    button,
                    false,
                    u32::from(action.release_dwell_ms()),
                    false,
                )];
                let batch = self
                    .run_batch(&release, true, false)
                    .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                absorb_batch_success(&mut progress, &batch);
                if !logical_button_released(
                    action.logical_button(),
                    batch.observation.logical_buttons_1_to_5,
                ) {
                    return Err(self.fail_observed_batch(&progress));
                }
                self.confirm_observed_batch(&progress)?;
                if traversed_all_segments && batch.observation.pointer != action.movement().end() {
                    return Err(progress.error(InputFailureKind::PostconditionFailed));
                }
                if traversed_all_segments {
                    progress.completed_units = 1;
                }
                Ok(progress)
            }
            InputAction::Scroll(action) => {
                let mut progress = ActionProgress::default();
                for notch in 0..action.count() {
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    self.drain_for_action(&progress)?;
                    if let Some(stopped) = boundary_stop(cancellation, deadline) {
                        progress.stopped = Some(stopped);
                        return Ok(progress);
                    }
                    let button = self
                        .resolve_button(action.logical_button())
                        .map_err(|kind| progress.error(kind))?;
                    progress.button_observation_partial = action.logical_button().number() > 5;
                    if self.state.pressed_buttons().contains(&button) {
                        return Err(progress.error(InputFailureKind::StateRejected));
                    }
                    let delay = if notch == 0 {
                        0
                    } else {
                        u32::from(action.interval_ms())
                    };
                    let batch = self
                        .run_batch(&button_pair(button, delay, 0), true, false)
                        .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                    progress.events_emitted += batch.sent;
                    progress.observed_pointer = Some(batch.observation.pointer);
                    progress.observed_buttons = Some(batch.observation.logical_buttons_1_to_5);
                    progress.button_observation_partial = action.logical_button().number() > 5;
                    if !logical_button_released(
                        action.logical_button(),
                        batch.observation.logical_buttons_1_to_5,
                    ) {
                        return Err(self.fail_observed_batch(&progress));
                    }
                    self.confirm_observed_batch(&progress)?;
                    progress.completed_units = progress.completed_units.saturating_add(1);
                    if deadline_elapsed(deadline) {
                        progress.stopped = Some(BoundaryStop::Deadline);
                        return Ok(progress);
                    }
                }
                Ok(progress)
            }
            InputAction::Button {
                button,
                direction,
                allow_redundant,
            } => {
                let mut progress = ActionProgress::default();
                if let Some(stopped) = boundary_stop(cancellation, deadline) {
                    progress.stopped = Some(stopped);
                    return Ok(progress);
                }
                let logical_before = logical_for_physical(&self.button_mapping, *button);
                progress.button_observation_partial = !matches!(logical_before, Some(1..=5));
                let planned = [button_event(
                    *button,
                    *direction == ButtonDirection::Down,
                    0,
                    *allow_redundant,
                )];
                let batch = self
                    .run_batch(&planned, false, false)
                    .map_err(|error| absorb_batch_failure(&mut progress, &error))?;
                progress.events_emitted = batch.sent;
                progress.observed_pointer = Some(batch.observation.pointer);
                progress.observed_buttons = Some(batch.observation.logical_buttons_1_to_5);
                let logical = if batch.mapping_changed {
                    None
                } else {
                    logical_for_physical(&self.button_mapping, *button)
                };
                progress.button_observation_partial = !matches!(logical, Some(1..=5));
                if let Some(logical @ 1..=5) = logical {
                    let expected = *direction == ButtonDirection::Down;
                    if batch.observation.logical_buttons_1_to_5[usize::from(logical - 1)]
                        != expected
                    {
                        return Err(self.fail_observed_batch(&progress));
                    }
                }
                self.confirm_observed_batch(&progress)?;
                progress.completed_units = 1;
                progress.stopped = deadline_stop(deadline);
                Ok(progress)
            }
            InputAction::Key(_) => {
                Err(ActionProgress::default().error(InputFailureKind::UnsupportedOperation))
            }
        }
    }

    fn run_window_click_dwell(&self, dwell_ms: u16) {
        if dwell_ms > 0 {
            self.backend
                .wait_for_input_delay(std::time::Duration::from_millis(u64::from(dwell_ms)));
        }
    }

    fn revalidate_window_click(
        &mut self,
        staged: &mut WindowClickRevalidation,
        progress: &mut ActionProgress,
    ) -> Result<(), ActionError> {
        let live_target = self
            .resolve_window_click_target(staged.request)
            .map_err(|kind| progress.error(kind))?;
        if live_target != staged.expected_target {
            return Err(progress.error(InputFailureKind::PostconditionFailed));
        }
        let observation = self.backend.observe_pointer().map_err(|fault| {
            self.apply_backend_fault(&fault);
            progress.error(backend_public_kind(&fault))
        })?;
        self.last_pointer = Some(observation.pointer);
        progress.observed_pointer = Some(observation.pointer);
        progress.observed_buttons = Some(observation.logical_buttons_1_to_5);
        if observation.pointer != live_target {
            return Err(progress.error(InputFailureKind::PostconditionFailed));
        }
        if let Some(precondition) = staged.precondition.as_mut() {
            precondition
                .evaluate()
                .map_err(|failure| progress.error(input_precondition_failure_kind(failure)))?;
        }
        Ok(())
    }

    fn run_batch(
        &mut self,
        events: &[PlannedEvent],
        mapping_sensitive: bool,
        confirm_now: bool,
    ) -> Result<BatchSuccess, BatchFailure> {
        let fail_next_state_mutation = {
            #[cfg(test)]
            {
                std::mem::take(&mut self.fail_next_state_mutation)
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        let backend = &self.backend;
        let state = &mut self.state;
        let sent_press_ledger = &mut self.sent_press_ledger;
        let mut cookies = Vec::with_capacity(events.len());
        let mut attempted = 0_usize;
        let mut sent = 0_usize;
        let mut recorded = 0_usize;
        let mut send_failure = None;
        let mut state_failure = false;
        for event in events {
            attempted = attempted.saturating_add(1);
            sent_press_ledger.note(event.backend, event.effect);
            match backend.send_event(event.backend) {
                Ok(cookie) => {
                    cookies.push(cookie);
                    sent = sent.saturating_add(1);
                    let tracked_effect = !matches!(event.effect, StateEffect::UntrackedRelease);
                    let state_result = if fail_next_state_mutation && sent == 1 && tracked_effect {
                        Err(InputStateError::NoActiveAction)
                    } else {
                        apply_effect(state, event.effect)
                    };
                    if state_result.is_err() {
                        state_failure = true;
                        break;
                    }
                    if tracked_effect {
                        recorded = recorded.saturating_add(1);
                    }
                }
                Err(fault) => {
                    send_failure = Some(fault);
                    break;
                }
            }
        }

        let mut check_failure = None;
        for cookie in cookies {
            if let Err(fault) = B::check_cookie(cookie)
                && check_failure.as_ref().is_none_or(|current: &BackendFault| {
                    current.kind != BackendFaultKind::Connection
                        || fault.kind == BackendFaultKind::Connection
                })
            {
                check_failure = Some(fault);
            }
        }
        let connection_failure = send_failure
            .as_ref()
            .is_some_and(|fault| fault.kind == BackendFaultKind::Connection)
            || check_failure
                .as_ref()
                .is_some_and(|fault| fault.kind == BackendFaultKind::Connection);
        let any_backend_failure = send_failure.is_some() || check_failure.is_some();
        if state_failure || any_backend_failure {
            let kind = if state_failure {
                BatchFailureKind::State
            } else if connection_failure {
                BatchFailureKind::Connection
            } else {
                BatchFailureKind::CheckedRequest
            };
            apply_batch_uncertainty(state, kind, recorded, sent);
            return Err(BatchFailure {
                kind,
                attempted,
                sent,
                observation: None,
            });
        }

        let observation = match backend.observe_pointer() {
            Ok(observation) => observation,
            Err(fault) => {
                let kind = if fault.kind == BackendFaultKind::Connection {
                    BatchFailureKind::Connection
                } else {
                    BatchFailureKind::Barrier
                };
                apply_batch_uncertainty(state, kind, recorded, sent);
                return Err(BatchFailure {
                    kind,
                    attempted,
                    sent,
                    observation: None,
                });
            }
        };
        self.last_pointer = Some(observation.pointer);
        self.sent_press_ledger
            .reconcile_buttons(&self.button_mapping, observation.logical_buttons_1_to_5);
        let drained = match backend.drain_events() {
            Ok(drained) => drained,
            Err(fault) => {
                let kind = if fault.kind == BackendFaultKind::Connection {
                    BatchFailureKind::Connection
                } else {
                    BatchFailureKind::CheckedRequest
                };
                apply_batch_uncertainty(state, kind, recorded, sent);
                return Err(BatchFailure {
                    kind,
                    attempted,
                    sent,
                    observation: Some(observation),
                });
            }
        };
        let pointer_mapping_changed = drained.pointer_mapping.is_some();
        apply_drained_mapping(&mut self.button_mapping, drained);
        if mapping_sensitive && pointer_mapping_changed {
            apply_batch_uncertainty(state, BatchFailureKind::MappingChanged, recorded, sent);
            return Err(BatchFailure {
                kind: BatchFailureKind::MappingChanged,
                attempted,
                sent,
                observation: Some(observation),
            });
        }
        if confirm_now && recorded > 0 && state.confirm_batch().is_err() {
            apply_batch_uncertainty(state, BatchFailureKind::State, recorded, sent);
            return Err(BatchFailure {
                kind: BatchFailureKind::State,
                attempted,
                sent,
                observation: Some(observation),
            });
        }
        for event in events {
            if let BackendEvent::Button {
                button,
                pressed: false,
                ..
            } = event.backend
            {
                set_bit(&mut self.sent_press_ledger.buttons, button.detail(), false);
            }
        }
        Ok(BatchSuccess {
            attempted,
            sent,
            observation,
            mapping_changed: pointer_mapping_changed,
        })
    }

    fn drain_events(&mut self) -> Result<(), BackendFault> {
        let drained = self.backend.drain_events()?;
        apply_drained_mapping(&mut self.button_mapping, drained);
        Ok(())
    }

    fn drain_for_action(&mut self, progress: &ActionProgress) -> Result<(), ActionError> {
        match self.drain_events() {
            Ok(()) => Ok(()),
            Err(fault) => {
                self.apply_backend_fault(&fault);
                Err(progress.error(backend_public_kind(&fault)))
            }
        }
    }

    fn resolve_button(&self, logical: LogicalButton) -> Result<PhysicalButton, InputFailureKind> {
        self.button_mapping
            .physical_for(logical)
            .map_err(|_| InputFailureKind::UnsupportedByBackend)
    }

    fn require_observed_start(
        &mut self,
        plan: &xenoteer_core::input::MotionPlan,
        progress: &mut ActionProgress,
    ) -> Result<(), ActionError> {
        let observation = match self.backend.observe_pointer() {
            Ok(observation) => observation,
            Err(fault) => {
                if fault.kind == BackendFaultKind::Connection {
                    self.apply_backend_fault(&fault);
                } else {
                    let _ignored = self
                        .state
                        .transition_health(HealthEvent::RequireReset(ResetReason::BarrierFailed));
                }
                return Err(
                    progress.error(if fault.kind == BackendFaultKind::Connection {
                        InputFailureKind::BackendUnavailable
                    } else {
                        InputFailureKind::BarrierFailed
                    }),
                );
            }
        };
        self.last_pointer = Some(observation.pointer);
        progress.observed_pointer = Some(observation.pointer);
        progress.observed_buttons = Some(observation.logical_buttons_1_to_5);
        if observation.pointer != plan.start() {
            return Err(progress.error(InputFailureKind::PostconditionFailed));
        }
        Ok(())
    }

    fn confirm_observed_batch(&mut self, progress: &ActionProgress) -> Result<(), ActionError> {
        if self.state.confirm_batch().is_err() {
            self.mark_panicked();
            Err(progress.error(InputFailureKind::ActorPanicked))
        } else {
            Ok(())
        }
    }

    fn fail_observed_batch(&mut self, progress: &ActionProgress) -> ActionError {
        if self
            .state
            .fail_batch(ResetReason::PostconditionFailed)
            .is_err()
        {
            self.mark_panicked();
            progress.error(InputFailureKind::ActorPanicked)
        } else {
            progress.error(InputFailureKind::PostconditionFailed)
        }
    }

    fn preflight(&self, action: &InputAction) -> Result<(), InputFailureKind> {
        if let InputAction::Button {
            button,
            direction,
            allow_redundant,
        } = action
            && !allow_redundant
        {
            let owned = self.state.pressed_buttons().contains(button);
            if (*direction == ButtonDirection::Down && owned)
                || (*direction == ButtonDirection::Up && !owned)
            {
                return Err(InputFailureKind::StateRejected);
            }
        }
        Ok(())
    }

    fn apply_backend_fault(&mut self, fault: &BackendFault) {
        if fault.kind == BackendFaultKind::Connection {
            let _ignored = self.state.transition_health(HealthEvent::ConnectionLost);
        } else {
            let _ignored = self
                .state
                .transition_health(HealthEvent::RequireReset(ResetReason::CheckedRequestFailed));
        }
    }

    fn failure_before(
        &self,
        context: ActionContext,
        kind: InputFailureKind,
        requested_pointer: Option<xenoteer_core::domain::RootPoint>,
    ) -> InputFailure {
        InputFailure {
            command_id: Some(context.command_id),
            kind,
            events_emitted: 0,
            completed_units: 0,
            progress_known: true,
            requested_pointer,
            last_observed_pointer: self.last_pointer,
            observed_logical_buttons_1_to_5: None,
            button_observation_partial: false,
            effects: None,
            cleanup: None,
            keyboard: None,
        }
    }

    fn finish_failure(
        &self,
        context: ActionContext,
        error: ActionError,
        evidence: Option<(EffectJournal, Option<Box<InputCleanupEvidence>>)>,
    ) -> InputFailure {
        let (effects, cleanup) = match evidence {
            Some((effects, cleanup)) => (Some(Box::new(effects.into())), cleanup),
            None => (None, None),
        };
        InputFailure {
            command_id: Some(context.command_id),
            kind: error.kind,
            events_emitted: error.events_emitted,
            completed_units: error.completed_units,
            progress_known: error.progress_known,
            requested_pointer: error.requested_pointer,
            last_observed_pointer: error.last_observed_pointer,
            observed_logical_buttons_1_to_5: error.observed_buttons,
            button_observation_partial: error.button_observation_partial,
            effects,
            cleanup,
            keyboard: None,
        }
    }

    fn invariant_failure(
        &self,
        context: ActionContext,
        requested_pointer: Option<xenoteer_core::domain::RootPoint>,
        cleanup: Option<CleanupReport>,
    ) -> InputFailure {
        InputFailure {
            command_id: Some(context.command_id),
            kind: InputFailureKind::ActorPanicked,
            events_emitted: 0,
            completed_units: 0,
            progress_known: false,
            requested_pointer,
            last_observed_pointer: self.last_pointer,
            observed_logical_buttons_1_to_5: None,
            button_observation_partial: false,
            effects: None,
            cleanup: cleanup.map(|report| Box::new(report.into())),
            keyboard: None,
        }
    }

    fn invariant_actor_failure(&self, cleanup: Option<CleanupReport>) -> InputFailure {
        let mut failure = self.actor_failure(InputFailureKind::ActorPanicked, cleanup);
        failure.progress_known = false;
        failure
    }

    fn actor_failure(
        &self,
        kind: InputFailureKind,
        cleanup: Option<CleanupReport>,
    ) -> InputFailure {
        InputFailure {
            command_id: None,
            kind,
            events_emitted: 0,
            completed_units: 0,
            progress_known: kind != InputFailureKind::ActorPanicked,
            requested_pointer: None,
            last_observed_pointer: self.last_pointer,
            observed_logical_buttons_1_to_5: None,
            button_observation_partial: false,
            effects: None,
            cleanup: cleanup.map(|report| Box::new(report.into())),
            keyboard: None,
        }
    }

    fn snapshot(&self, thread: ActorThreadState) -> InputHealthSnapshot {
        InputHealthSnapshot {
            input: self.state.health(),
            thread,
            button_mapping: Some(self.button_mapping.clone()),
            min_keycode: self.min_keycode,
            max_keycode: self.max_keycode,
            keyboard_model: self.keyboard_diagnostics(false),
        }
    }

    fn keyboard_diagnostics(&self, redact_scalar: bool) -> super::KeyboardModelDiagnostics {
        let mut diagnostics = self.keyboard.diagnostics();
        if redact_scalar || self.pending_keyboard_restore.is_some() {
            diagnostics.keymap_fingerprint = None;
        }
        diagnostics
    }

    fn cleanup_report(&self, inputs: CleanupReportInputs) -> CleanupReport {
        let residual = self.conservative_cleanup_actions();
        let mut residual_owned_buttons = self.state.pressed_buttons().to_vec();
        for button in residual.iter().filter_map(|action| match action {
            CleanupAction::ReleaseButton { button } => Some(*button),
            CleanupAction::ReleaseKey { .. } => None,
        }) {
            if !residual_owned_buttons.contains(&button) {
                residual_owned_buttons.push(button);
            }
        }
        let mut residual_owned_keys: Vec<_> = self
            .state
            .pressed_keys()
            .iter()
            .map(|owned| owned.key())
            .collect();
        for key in residual.iter().filter_map(|action| match action {
            CleanupAction::ReleaseKey { key, .. } => Some(*key),
            CleanupAction::ReleaseButton { .. } => None,
        }) {
            if !residual_owned_keys.contains(&key) {
                residual_owned_keys.push(key);
            }
        }
        CleanupReport {
            attempted: inputs.attempted,
            confirmed: inputs.confirmed,
            observed_logical_buttons_1_to_5: inputs
                .pointer
                .map(|observation| observation.logical_buttons_1_to_5),
            unobservable_buttons: inputs.unobservable_buttons,
            observed_pressed_keys: inputs.keys,
            residual_owned_buttons,
            residual_owned_keys,
            temporary_mapping_restore_attempted: inputs.temporary_mapping_restore_attempted,
            temporary_mapping_restore_proven: inputs.temporary_mapping_restore_proven,
            succeeded: inputs.succeeded,
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot_for_test(&self) -> InputHealthSnapshot {
        self.snapshot(ActorThreadState::Running)
    }

    #[cfg(test)]
    pub(super) fn fail_next_state_mutation_for_test(&mut self) {
        self.fail_next_state_mutation = true;
    }

    #[cfg(test)]
    pub(super) fn poison_connection_for_test(&mut self) -> Result<(), InputStateError> {
        self.state.transition_health(HealthEvent::ConnectionLost)
    }

    #[cfg(test)]
    pub(super) fn seed_owned_key_for_test(
        &mut self,
        key: xenoteer_core::input::PhysicalKey,
        modifier: bool,
    ) -> Result<(), InputStateError> {
        self.state.begin_action(ActionPurpose::Ordinary)?;
        self.state.submit_key_press(key, modifier)?;
        self.state.confirm_batch()?;
        let _journal = self.state.finish_action()?;
        Ok(())
    }
}

impl ActionProgress {
    fn error(&self, kind: InputFailureKind) -> ActionError {
        ActionError {
            kind,
            events_emitted: self.events_emitted,
            completed_units: self.completed_units,
            requested_pointer: self.requested_pointer,
            last_observed_pointer: self.observed_pointer,
            observed_buttons: self.observed_buttons,
            button_observation_partial: self.button_observation_partial,
            progress_known: kind != InputFailureKind::ActorPanicked,
        }
    }
}

const fn input_precondition_failure_kind(failure: InputPreconditionFailure) -> InputFailureKind {
    match failure {
        InputPreconditionFailure::TargetStale => InputFailureKind::TargetStale,
        InputPreconditionFailure::FocusLost => InputFailureKind::FocusLost,
        InputPreconditionFailure::Unavailable => InputFailureKind::PreconditionUnavailable,
    }
}

fn motion_sample_events(samples: &[xenoteer_core::input::MotionSample]) -> Vec<PlannedEvent> {
    samples
        .iter()
        .map(|sample| PlannedEvent {
            backend: BackendEvent::Motion {
                point: sample.point(),
                delay_ms: sample.delay_ms(),
            },
            effect: StateEffect::Motion(sample.point()),
        })
        .collect()
}

fn absorb_batch_success(progress: &mut ActionProgress, batch: &BatchSuccess) {
    progress.events_emitted = progress.events_emitted.saturating_add(batch.sent);
    progress.observed_pointer = Some(batch.observation.pointer);
    progress.observed_buttons = Some(batch.observation.logical_buttons_1_to_5);
}

fn absorb_batch_failure(progress: &mut ActionProgress, failure: &BatchFailure) -> ActionError {
    progress.events_emitted = progress.events_emitted.saturating_add(failure.sent);
    if let Some(observation) = &failure.observation {
        progress.observed_pointer = Some(observation.pointer);
        progress.observed_buttons = Some(observation.logical_buttons_1_to_5);
    }
    if matches!(failure.kind, BatchFailureKind::MappingChanged) {
        progress.button_observation_partial = true;
    }
    progress.error(failure.public_kind())
}

fn button_pair(button: PhysicalButton, press_delay: u32, release_delay: u32) -> [PlannedEvent; 2] {
    [
        button_event(button, true, press_delay, false),
        button_event(button, false, release_delay, false),
    ]
}

fn key_planned_event(
    key: xenoteer_core::input::PhysicalKey,
    pressed: bool,
    delay_ms: u32,
    modifier: bool,
) -> PlannedEvent {
    PlannedEvent {
        backend: BackendEvent::Key {
            key,
            pressed,
            delay_ms,
        },
        effect: if pressed {
            StateEffect::KeyPress { key, modifier }
        } else {
            StateEffect::KeyRelease(key)
        },
    }
}

fn has_duplicate_keys(keys: &[xenoteer_core::input::PhysicalKey]) -> bool {
    keys.iter()
        .enumerate()
        .any(|(index, key)| keys[..index].contains(key))
}

fn button_event(
    button: PhysicalButton,
    pressed: bool,
    delay_ms: u32,
    allow_redundant: bool,
) -> PlannedEvent {
    PlannedEvent {
        backend: BackendEvent::Button {
            button,
            pressed,
            delay_ms,
        },
        effect: if pressed {
            StateEffect::ButtonPress {
                button,
                allow_redundant,
            }
        } else {
            StateEffect::ButtonRelease {
                button,
                allow_redundant,
            }
        },
    }
}

fn apply_effect(state: &mut InputState, effect: StateEffect) -> Result<(), InputStateError> {
    match effect {
        StateEffect::Motion(point) => state.submit_pointer_motion(point),
        StateEffect::ButtonPress {
            button,
            allow_redundant,
        } => state.submit_button_press(button, allow_redundant),
        StateEffect::ButtonRelease {
            button,
            allow_redundant,
        } => state.submit_button_release(button, allow_redundant),
        StateEffect::KeyRelease(key) => state.submit_key_release(key),
        StateEffect::KeyPress { key, modifier } => state.submit_key_press(key, modifier),
        StateEffect::UntrackedRelease => Ok(()),
    }
}

fn apply_batch_uncertainty(
    state: &mut InputState,
    kind: BatchFailureKind,
    recorded: usize,
    _sent: usize,
) {
    if matches!(kind, BatchFailureKind::State) {
        let _ignored = state.transition_health(HealthEvent::ActorPanicked);
        return;
    }
    if matches!(kind, BatchFailureKind::Connection) {
        let _ignored = state.transition_health(HealthEvent::ConnectionLost);
        return;
    }
    let reason = match kind {
        BatchFailureKind::Barrier => ResetReason::BarrierFailed,
        BatchFailureKind::MappingChanged => ResetReason::PostconditionFailed,
        BatchFailureKind::Connection
        | BatchFailureKind::CheckedRequest
        | BatchFailureKind::State => ResetReason::CheckedRequestFailed,
    };
    if recorded > 0 {
        let _ignored = state.fail_batch(reason);
    } else {
        let _ignored = state.transition_health(HealthEvent::RequireReset(reason));
    }
}

fn apply_drained_mapping(
    mapping: &mut xenoteer_core::input::ButtonMapping,
    drained: DrainedEvents,
) {
    if let Some(updated) = drained.pointer_mapping {
        *mapping = updated;
    }
    let _keyboard_change = (
        drained.keyboard_mapping_changed,
        drained.xkb_model_changed,
        drained.xkb_state_changed,
    );
}

fn backend_public_kind(fault: &BackendFault) -> InputFailureKind {
    if fault.kind == BackendFaultKind::Connection {
        InputFailureKind::BackendUnavailable
    } else {
        InputFailureKind::CheckedRequestFailed
    }
}

pub(super) fn requested_pointer(
    operation: &InputOperation,
) -> Option<xenoteer_core::domain::RootPoint> {
    match operation {
        InputOperation::Pointer(action) => requested_pointer_action(action),
        InputOperation::PointerMove(request) => Some(request.target()),
        InputOperation::PointerMoveRelative(_)
        | InputOperation::PointerClick(_)
        | InputOperation::PointerDrag(_)
        | InputOperation::WindowPointerClick(_)
        | InputOperation::PointerScroll(_)
        | InputOperation::Keyboard(_) => None,
    }
}

fn requested_pointer_action(action: &InputAction) -> Option<xenoteer_core::domain::RootPoint> {
    match action {
        InputAction::Move(action) => Some(action.plan().end()),
        InputAction::Click(action) => action.movement().map(|movement| movement.end()),
        InputAction::Drag(action) => Some(action.movement().end()),
        InputAction::Scroll(_) | InputAction::Key(_) | InputAction::Button { .. } => None,
    }
}

fn deadline_elapsed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

const fn unicode_keysym(scalar: char) -> u32 {
    let value = scalar as u32;
    if matches!(value, 0x20..=0x7e | 0xa0..=0xff) {
        value
    } else {
        0x0100_0000 | value
    }
}

fn set_bit(bits: &mut [u64; 4], detail: u8, value: bool) {
    let index = usize::from(detail / 64);
    let mask = 1_u64 << (detail % 64);
    if value {
        bits[index] |= mask;
    } else {
        bits[index] &= !mask;
    }
}

fn bit_is_set(bits: &[u64; 4], detail: u8) -> bool {
    let index = usize::from(detail / 64);
    bits[index] & (1_u64 << (detail % 64)) != 0
}

fn boundary_stop(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Option<BoundaryStop> {
    if cancellation.is_cancelled() {
        Some(BoundaryStop::Cancelled)
    } else if deadline_elapsed(deadline) {
        Some(BoundaryStop::Deadline)
    } else {
        None
    }
}

fn deadline_stop(deadline: Option<Instant>) -> Option<BoundaryStop> {
    deadline_elapsed(deadline).then_some(BoundaryStop::Deadline)
}

fn logical_button_released(logical: LogicalButton, observed: [bool; 5]) -> bool {
    let number = logical.number();
    number > 5 || !observed[usize::from(number - 1)]
}

fn logical_button_pressed(logical: LogicalButton, observed: [bool; 5]) -> bool {
    let number = logical.number();
    number > 5 || observed[usize::from(number - 1)]
}

fn logical_for_physical(
    mapping: &xenoteer_core::input::ButtonMapping,
    physical: PhysicalButton,
) -> Option<u8> {
    mapping
        .as_server_map()
        .get(usize::from(physical.detail()).checked_sub(1)?)
        .copied()
}
