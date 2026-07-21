//! Owned physical input state and explicit health transitions.

use thiserror::Error;

use crate::domain::RootPoint;

use super::effect::EffectCheckpoint;
use super::{Effect, EffectJournal, EffectJournalError, PhysicalButton, PhysicalKey};

/// Why ordinary input must stop until conservative reset completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// A retained X request cookie reported failure.
    CheckedRequestFailed,
    /// The same-connection observation barrier failed.
    BarrierFailed,
    /// Observed state did not meet a required postcondition.
    PostconditionFailed,
    /// Cancellation occurred after a physical effect began.
    CancelledAfterEffect,
}

/// Why an input actor cannot safely return to service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonReason {
    /// The X connection was lost with effects potentially outstanding.
    ConnectionLost,
    /// Conservative reset or its observation proof failed.
    ResetFailed,
    /// The actor thread unwound unexpectedly.
    ActorPanicked,
    /// Exact restoration of a temporary keyboard mapping could not be proved.
    TemporaryKeyboardMappingRestoreFailed,
}

/// Whether the actor can accept ordinary input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputHealth {
    /// Ordinary input may execute.
    Healthy,
    /// Only reset/observation work may execute.
    ResetRequired(ResetReason),
    /// Restart is required before ordinary input can resume.
    Poisoned(PoisonReason),
}

/// An explicit event in the input-health state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthEvent {
    /// Mark state uncertain and require conservative reset.
    RequireReset(ResetReason),
    /// Record a fully observed successful reset.
    ResetSucceeded,
    /// Record a failed reset attempt.
    ResetFailed,
    /// Record loss of the X connection.
    ConnectionLost,
    /// Record an actor panic caught at its thread boundary.
    ActorPanicked,
    /// Record unproved restoration of an actor-installed temporary key mapping.
    TemporaryKeyboardMappingRestoreFailed,
}

/// Why a command-scoped effect journal is being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPurpose {
    /// User-requested input, admitted only while healthy.
    Ordinary,
    /// Conservative release/reset work, admitted in any health state.
    Reset,
}

/// One owned key and its XKB modifier classification at press time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnedKey {
    key: PhysicalKey,
    modifier: bool,
}

impl OwnedKey {
    /// Returns the physical key.
    #[must_use]
    pub const fn key(self) -> PhysicalKey {
        self.key
    }

    /// Returns whether this key was classified as a modifier at press time.
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        self.modifier
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRelease {
    Button(PhysicalButton),
    Key(PhysicalKey),
}

/// Actor-owned presses, effect evidence, and health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputState {
    pressed_buttons: Vec<PhysicalButton>,
    pressed_keys: Vec<OwnedKey>,
    effects: Option<EffectJournal>,
    action_purpose: Option<ActionPurpose>,
    batch_checkpoint: Option<EffectCheckpoint>,
    pending_releases: Vec<PendingRelease>,
    health: InputHealth,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    /// Creates healthy input state with no Xenoteer-owned presses.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pressed_buttons: Vec::new(),
            pressed_keys: Vec::new(),
            effects: None,
            action_purpose: None,
            batch_checkpoint: None,
            pending_releases: Vec::new(),
            health: InputHealth::Healthy,
        }
    }

    /// Returns current actor health.
    #[must_use]
    pub const fn health(&self) -> InputHealth {
        self.health
    }

    /// Returns buttons in successful serialization/press order.
    #[must_use]
    pub fn pressed_buttons(&self) -> &[PhysicalButton] {
        &self.pressed_buttons
    }

    /// Returns keys in successful serialization/press order.
    #[must_use]
    pub fn pressed_keys(&self) -> &[OwnedKey] {
        &self.pressed_keys
    }

    /// Returns all effect evidence accumulated for this state generation.
    #[must_use]
    pub const fn effects(&self) -> Option<&EffectJournal> {
        self.effects.as_ref()
    }

    /// Starts one command-scoped effect journal.
    pub fn begin_action(&mut self, purpose: ActionPurpose) -> Result<(), InputStateError> {
        if self.effects.is_some() {
            return Err(InputStateError::ActionAlreadyActive);
        }
        if self.batch_checkpoint.is_some() || !self.pending_releases.is_empty() {
            return Err(InputStateError::UnresolvedBatch);
        }
        if purpose == ActionPurpose::Ordinary && self.health != InputHealth::Healthy {
            return Err(InputStateError::ActionPurposeNotAllowed {
                purpose,
                health: self.health,
            });
        }
        self.effects = Some(EffectJournal::new());
        self.action_purpose = Some(purpose);
        Ok(())
    }

    /// Ends one action after every submitted batch was confirmed or abandoned.
    ///
    /// Historical provisional records from an abandoned batch remain in the
    /// returned journal as uncertainty evidence; they do not leak into the next
    /// command. Persistent owned presses remain in `InputState`.
    pub fn finish_action(&mut self) -> Result<EffectJournal, InputStateError> {
        if self.batch_checkpoint.is_some() || !self.pending_releases.is_empty() {
            return Err(InputStateError::UnresolvedBatch);
        }
        let journal = self.effects.take().ok_or(InputStateError::NoActiveAction)?;
        self.action_purpose = None;
        Ok(journal)
    }

    /// Records one serialized pointer motion in the current ordinary action.
    pub fn submit_pointer_motion(&mut self, point: RootPoint) -> Result<(), InputStateError> {
        self.ensure_press_allowed()?;
        self.append_effect(Effect::PointerMoved { point })?;
        Ok(())
    }

    fn append_effect(&mut self, effect: Effect) -> Result<u64, InputStateError> {
        let effects = self
            .effects
            .as_mut()
            .ok_or(InputStateError::NoActiveAction)?;
        if self.batch_checkpoint.is_none() {
            self.batch_checkpoint = Some(effects.checkpoint());
        }
        effects
            .record_provisional(effect)
            .map_err(InputStateError::EffectJournal)
    }

    /// Records a serialized button press and conservatively assumes ownership.
    pub fn submit_button_press(
        &mut self,
        button: PhysicalButton,
        allow_redundant: bool,
    ) -> Result<(), InputStateError> {
        self.ensure_press_allowed()?;
        if self.pressed_buttons.contains(&button) {
            if !allow_redundant {
                return Err(InputStateError::ButtonAlreadyPressed { button });
            }
            self.append_effect(Effect::ButtonPressed { button })?;
            return Ok(());
        }
        self.append_effect(Effect::ButtonPressed { button })?;
        self.pressed_buttons.push(button);
        Ok(())
    }

    /// Records a serialized button release but retains ownership until confirmation.
    pub fn submit_button_release(
        &mut self,
        button: PhysicalButton,
        allow_redundant: bool,
    ) -> Result<(), InputStateError> {
        let is_owned = self.pressed_buttons.contains(&button);
        let already_pending = self
            .pending_releases
            .contains(&PendingRelease::Button(button));
        if (!is_owned || already_pending) && !allow_redundant {
            return Err(InputStateError::ButtonNotPressed { button });
        }
        self.append_effect(Effect::ButtonReleased { button })?;
        if is_owned && !already_pending {
            self.pending_releases.push(PendingRelease::Button(button));
        }
        Ok(())
    }

    /// Records a serialized key press and conservatively assumes ownership.
    pub fn submit_key_press(
        &mut self,
        key: PhysicalKey,
        modifier: bool,
    ) -> Result<(), InputStateError> {
        self.ensure_press_allowed()?;
        if self.pressed_keys.iter().any(|owned| owned.key == key) {
            return Err(InputStateError::KeyAlreadyPressed { key });
        }
        self.append_effect(Effect::KeyPressed { key, modifier })?;
        self.pressed_keys.push(OwnedKey { key, modifier });
        Ok(())
    }

    /// Records a serialized key release but retains ownership until confirmation.
    pub fn submit_key_release(&mut self, key: PhysicalKey) -> Result<(), InputStateError> {
        let owned = self
            .pressed_keys
            .iter()
            .find(|owned| owned.key == key)
            .copied()
            .ok_or(InputStateError::KeyNotPressed { key })?;
        if self.pending_releases.contains(&PendingRelease::Key(key)) {
            return Err(InputStateError::KeyNotPressed { key });
        }
        self.append_effect(Effect::KeyReleased {
            key,
            modifier: owned.modifier,
        })?;
        self.pending_releases.push(PendingRelease::Key(key));
        Ok(())
    }

    /// Confirms only the current checked-cookie/barrier batch and applies its releases.
    pub fn confirm_batch(&mut self) -> Result<(), InputStateError> {
        let checkpoint = self
            .batch_checkpoint
            .ok_or(InputStateError::NoPendingBatch)?;
        self.effects
            .as_mut()
            .ok_or(InputStateError::NoActiveAction)?
            .confirm_since(checkpoint)
            .map_err(InputStateError::EffectJournal)?;
        for release in self.pending_releases.drain(..) {
            match release {
                PendingRelease::Button(button) => {
                    if let Some(index) = self
                        .pressed_buttons
                        .iter()
                        .position(|owned| *owned == button)
                    {
                        self.pressed_buttons.remove(index);
                    }
                }
                PendingRelease::Key(key) => {
                    if let Some(index) = self.pressed_keys.iter().position(|owned| owned.key == key)
                    {
                        self.pressed_keys.remove(index);
                    }
                }
            }
        }
        self.batch_checkpoint = None;
        Ok(())
    }

    /// Abandons an uncertain batch, keeps conservative ownership, and requires reset.
    pub fn fail_batch(&mut self, reason: ResetReason) -> Result<(), InputStateError> {
        if self.batch_checkpoint.is_none() {
            return Err(InputStateError::NoPendingBatch);
        }
        self.transition_health(HealthEvent::RequireReset(reason))?;
        self.pending_releases.clear();
        self.batch_checkpoint = None;
        Ok(())
    }

    /// Abandons a failed reset batch when health was already terminal.
    ///
    /// A poisoned actor may still attempt best-effort releases, but a failed
    /// checked request or barrier cannot legally transition it through
    /// `ResetRequired` again. This closes only the current batch bookkeeping:
    /// owned presses, provisional evidence, and the original poison reason are
    /// deliberately preserved.
    pub fn abandon_poisoned_reset_batch(&mut self) -> Result<(), InputStateError> {
        if self.batch_checkpoint.is_none() {
            return Err(InputStateError::NoPendingBatch);
        }
        if self.action_purpose != Some(ActionPurpose::Reset)
            || !matches!(self.health, InputHealth::Poisoned(_))
        {
            return Err(InputStateError::PoisonedBatchAbandonNotAllowed {
                purpose: self.action_purpose,
                health: self.health,
            });
        }
        self.pending_releases.clear();
        self.batch_checkpoint = None;
        Ok(())
    }

    /// Applies a legal health transition.
    pub fn transition_health(&mut self, event: HealthEvent) -> Result<(), InputStateError> {
        if matches!(
            event,
            HealthEvent::ResetSucceeded | HealthEvent::ResetFailed
        ) && (self.action_purpose != Some(ActionPurpose::Reset)
            || self.batch_checkpoint.is_some()
            || !self.pending_releases.is_empty())
        {
            return Err(InputStateError::ResetTransitionRequiresResetAction { event });
        }
        let next = match (self.health, event) {
            (InputHealth::Healthy, HealthEvent::RequireReset(reason))
            | (InputHealth::ResetRequired(_), HealthEvent::RequireReset(reason)) => {
                InputHealth::ResetRequired(reason)
            }
            (InputHealth::ResetRequired(_), HealthEvent::ResetSucceeded) => {
                if !self.pressed_buttons.is_empty()
                    || !self.pressed_keys.is_empty()
                    || !self.pending_releases.is_empty()
                {
                    return Err(InputStateError::ResetHasOwnedInput);
                }
                InputHealth::Healthy
            }
            (InputHealth::ResetRequired(_), HealthEvent::ResetFailed) => {
                InputHealth::Poisoned(PoisonReason::ResetFailed)
            }
            (InputHealth::Healthy | InputHealth::ResetRequired(_), HealthEvent::ConnectionLost) => {
                InputHealth::Poisoned(PoisonReason::ConnectionLost)
            }
            (InputHealth::Healthy | InputHealth::ResetRequired(_), HealthEvent::ActorPanicked) => {
                InputHealth::Poisoned(PoisonReason::ActorPanicked)
            }
            (
                InputHealth::Healthy | InputHealth::ResetRequired(_),
                HealthEvent::TemporaryKeyboardMappingRestoreFailed,
            ) => InputHealth::Poisoned(PoisonReason::TemporaryKeyboardMappingRestoreFailed),
            (current, attempted) => {
                return Err(InputStateError::IllegalHealthTransition { current, attempted });
            }
        };
        if matches!(
            event,
            HealthEvent::ConnectionLost
                | HealthEvent::ActorPanicked
                | HealthEvent::TemporaryKeyboardMappingRestoreFailed
        ) {
            // A terminal actor/connection failure cannot check the current
            // request batch. Preserve owned presses and provisional evidence,
            // but abandon release bookkeeping so the command journal can be
            // returned and the control slot cannot deadlock permanently.
            self.pending_releases.clear();
            self.batch_checkpoint = None;
        }
        self.health = next;
        Ok(())
    }

    fn ensure_press_allowed(&self) -> Result<(), InputStateError> {
        let purpose = self.action_purpose.ok_or(InputStateError::NoActiveAction)?;
        if purpose != ActionPurpose::Ordinary || self.health != InputHealth::Healthy {
            return Err(InputStateError::ActionPurposeNotAllowed {
                purpose,
                health: self.health,
            });
        }
        Ok(())
    }
}

/// Failure to update owned input or actor health safely.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InputStateError {
    /// Effect evidence could not be retained.
    #[error("effect journal update failed: {0}")]
    EffectJournal(EffectJournalError),
    /// A command attempted to start while another journal was active.
    #[error("an input action is already active")]
    ActionAlreadyActive,
    /// The action purpose cannot execute in the current health state.
    #[error("{purpose:?} input action is not allowed while health is {health:?}")]
    ActionPurposeNotAllowed {
        /// Rejected purpose.
        purpose: ActionPurpose,
        /// Current health.
        health: InputHealth,
    },
    /// An effect was submitted without an active command journal.
    #[error("no input action is active")]
    NoActiveAction,
    /// Confirmation or failure was requested without submitted effects.
    #[error("no input request batch is pending")]
    NoPendingBatch,
    /// An action cannot end while its request batch is unresolved.
    #[error("input request batch must be confirmed or abandoned before action end")]
    UnresolvedBatch,
    /// Reset completion must occur inside a resolved reset action.
    #[error("{event:?} requires an active reset action with no pending request batch")]
    ResetTransitionRequiresResetAction {
        /// Rejected reset outcome.
        event: HealthEvent,
    },
    /// Only an active reset may abandon a batch while health is already
    /// terminal.
    #[error(
        "poisoned batch abandonment requires an active reset action; purpose={purpose:?}, health={health:?}"
    )]
    PoisonedBatchAbandonNotAllowed {
        /// Current action purpose, if any.
        purpose: Option<ActionPurpose>,
        /// Current health.
        health: InputHealth,
    },
    /// A non-redundant press targeted an owned button.
    #[error("physical button {button:?} is already owned")]
    ButtonAlreadyPressed {
        /// Duplicate button.
        button: PhysicalButton,
    },
    /// A non-redundant release targeted an unowned or already-pending button.
    #[error("physical button {button:?} is not available for release")]
    ButtonNotPressed {
        /// Rejected button.
        button: PhysicalButton,
    },
    /// A press targeted an already-owned key.
    #[error("physical key {key:?} is already owned")]
    KeyAlreadyPressed {
        /// Duplicate key.
        key: PhysicalKey,
    },
    /// A release targeted an unowned or already-pending key.
    #[error("physical key {key:?} is not available for release")]
    KeyNotPressed {
        /// Rejected key.
        key: PhysicalKey,
    },
    /// A reset was declared successful before owned input was cleared.
    #[error("reset cannot succeed while owned input remains")]
    ResetHasOwnedInput,
    /// The requested health transition is not legal.
    #[error("illegal input-health transition from {current:?} using {attempted:?}")]
    IllegalHealthTransition {
        /// Current health.
        current: InputHealth,
        /// Rejected event.
        attempted: HealthEvent,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputActionError;

    #[test]
    fn failed_release_batch_retains_conservative_ownership()
    -> Result<(), Box<dyn std::error::Error>> {
        let button = PhysicalButton::new(1)?;
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary)?;
        state.submit_button_press(button, false)?;
        state.submit_button_release(button, false)?;
        state.fail_batch(ResetReason::BarrierFailed)?;
        assert_eq!(state.pressed_buttons(), &[button]);
        assert_eq!(
            state.health(),
            InputHealth::ResetRequired(ResetReason::BarrierFailed)
        );
        assert!(state.effects().is_some_and(EffectJournal::has_provisional));
        let journal = state.finish_action()?;
        assert!(journal.has_provisional());
        Ok(())
    }

    #[test]
    fn confirmed_release_clears_ownership() -> Result<(), Box<dyn std::error::Error>> {
        let key = PhysicalKey::new(38)?;
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary)?;
        state.submit_key_press(key, false)?;
        state.submit_key_release(key)?;
        state.confirm_batch()?;
        assert!(state.pressed_keys().is_empty());
        assert!(
            state
                .effects()
                .is_some_and(|journal| !journal.has_provisional())
        );
        let journal = state.finish_action()?;
        assert!(!journal.has_provisional());
        Ok(())
    }

    #[test]
    fn poisoned_health_is_terminal() -> Result<(), InputStateError> {
        let mut state = InputState::new();
        state.transition_health(HealthEvent::ConnectionLost)?;
        assert_eq!(
            state.begin_action(ActionPurpose::Ordinary),
            Err(InputStateError::ActionPurposeNotAllowed {
                purpose: ActionPurpose::Ordinary,
                health: InputHealth::Poisoned(PoisonReason::ConnectionLost)
            })
        );
        state.begin_action(ActionPurpose::Reset)?;
        let _journal = state.finish_action()?;
        assert_eq!(
            state.transition_health(HealthEvent::RequireReset(ResetReason::BarrierFailed)),
            Err(InputStateError::IllegalHealthTransition {
                current: InputHealth::Poisoned(PoisonReason::ConnectionLost),
                attempted: HealthEvent::RequireReset(ResetReason::BarrierFailed)
            })
        );
        Ok(())
    }

    #[test]
    fn failed_ordinary_action_cannot_resume_after_reset_transition()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary)?;
        state.submit_pointer_motion(RootPoint::new(1, 1)?)?;
        state.fail_batch(ResetReason::BarrierFailed)?;
        assert_eq!(
            state.transition_health(HealthEvent::ResetSucceeded),
            Err(InputStateError::ResetTransitionRequiresResetAction {
                event: HealthEvent::ResetSucceeded
            })
        );
        let _failed_journal = state.finish_action()?;
        state.begin_action(ActionPurpose::Reset)?;
        state.transition_health(HealthEvent::ResetSucceeded)?;
        let _reset_journal = state.finish_action()?;
        assert_eq!(state.health(), InputHealth::Healthy);
        Ok(())
    }

    #[test]
    fn terminal_poison_abandons_batch_but_preserves_ownership_and_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (HealthEvent::ConnectionLost, PoisonReason::ConnectionLost),
            (HealthEvent::ActorPanicked, PoisonReason::ActorPanicked),
        ];
        for (event, reason) in cases {
            let button = PhysicalButton::new(1)?;
            let mut state = InputState::new();
            state.begin_action(ActionPurpose::Ordinary)?;
            state.submit_button_press(button, false)?;
            state.submit_button_release(button, false)?;
            state.transition_health(event)?;

            assert_eq!(state.health(), InputHealth::Poisoned(reason));
            assert_eq!(state.pressed_buttons(), &[button]);
            assert!(state.effects().is_some_and(EffectJournal::has_provisional));
            let journal = state.finish_action()?;
            assert!(journal.has_provisional());
            assert_eq!(journal.len(), 2);
        }
        Ok(())
    }

    #[test]
    fn failed_cleanup_batch_can_close_without_replacing_existing_poison()
    -> Result<(), Box<dyn std::error::Error>> {
        let button = PhysicalButton::new(1)?;
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary)?;
        state.submit_button_press(button, false)?;
        state.confirm_batch()?;
        let _press_journal = state.finish_action()?;
        state.transition_health(HealthEvent::ConnectionLost)?;

        state.begin_action(ActionPurpose::Reset)?;
        state.submit_button_release(button, false)?;
        state.abandon_poisoned_reset_batch()?;

        assert_eq!(
            state.health(),
            InputHealth::Poisoned(PoisonReason::ConnectionLost)
        );
        assert_eq!(state.pressed_buttons(), &[button]);
        assert!(state.effects().is_some_and(EffectJournal::has_provisional));
        let cleanup_journal = state.finish_action()?;
        assert!(cleanup_journal.has_provisional());
        assert_eq!(cleanup_journal.len(), 1);
        Ok(())
    }

    #[test]
    fn action_error_type_stays_available() {
        assert_eq!(
            PhysicalButton::new(0),
            Err(InputActionError::InvalidPhysicalButton)
        );
    }

    #[test]
    fn many_sequential_action_journals_do_not_exhaust_lifetime_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let key = PhysicalKey::new(38)?;
        let mut state = InputState::new();
        for _ in 0..5_000 {
            state.begin_action(ActionPurpose::Ordinary)?;
            state.submit_key_press(key, false)?;
            state.submit_key_release(key)?;
            state.confirm_batch()?;
            let journal = state.finish_action()?;
            assert_eq!(journal.len(), 2);
        }
        assert!(state.pressed_keys().is_empty());
        Ok(())
    }
}
