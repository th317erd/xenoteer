//! Conservative release planning for Xenoteer-owned input.

use super::{InputState, PhysicalButton, PhysicalKey};

/// One zero-delay release in a reset plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupAction {
    /// Release an owned physical button.
    ReleaseButton {
        /// Button to release.
        button: PhysicalButton,
    },
    /// Release an owned physical key.
    ReleaseKey {
        /// Key to release.
        key: PhysicalKey,
        /// Whether it was classified as a modifier when pressed.
        modifier: bool,
    },
}

/// A snapshot of releases to attempt before an observation barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPlan {
    actions: Vec<CleanupAction>,
}

impl CleanupPlan {
    /// Returns releases in required send order.
    #[must_use]
    pub fn actions(&self) -> &[CleanupAction] {
        &self.actions
    }

    /// Returns the release count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Returns whether there is no Xenoteer-owned input to release.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Snapshots reverse-chronological button, non-modifier, then modifier releases.
///
/// Planning never mutates ownership. The actor submits these releases, checks
/// every cookie plus its observation barrier, and only then confirms the batch.
#[must_use]
pub fn plan_cleanup(state: &InputState) -> CleanupPlan {
    let mut actions = Vec::with_capacity(
        state
            .pressed_buttons()
            .len()
            .saturating_add(state.pressed_keys().len()),
    );
    actions.extend(
        state
            .pressed_buttons()
            .iter()
            .rev()
            .map(|button| CleanupAction::ReleaseButton { button: *button }),
    );
    actions.extend(
        state
            .pressed_keys()
            .iter()
            .rev()
            .filter(|owned| !owned.is_modifier())
            .map(|owned| CleanupAction::ReleaseKey {
                key: owned.key(),
                modifier: false,
            }),
    );
    actions.extend(
        state
            .pressed_keys()
            .iter()
            .rev()
            .filter(|owned| owned.is_modifier())
            .map(|owned| CleanupAction::ReleaseKey {
                key: owned.key(),
                modifier: true,
            }),
    );
    CleanupPlan { actions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::ActionPurpose;

    #[test]
    fn cleanup_partitions_categories_and_reverses_each_press_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let button_one = PhysicalButton::new(1)?;
        let button_two = PhysicalButton::new(3)?;
        let modifier_one = PhysicalKey::new(37)?;
        let key_one = PhysicalKey::new(38)?;
        let modifier_two = PhysicalKey::new(50)?;
        let key_two = PhysicalKey::new(40)?;
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary)?;
        state.submit_button_press(button_one, false)?;
        state.submit_key_press(modifier_one, true)?;
        state.submit_key_press(key_one, false)?;
        state.submit_button_press(button_two, false)?;
        state.submit_key_press(modifier_two, true)?;
        state.submit_key_press(key_two, false)?;

        assert_eq!(
            plan_cleanup(&state).actions(),
            &[
                CleanupAction::ReleaseButton { button: button_two },
                CleanupAction::ReleaseButton { button: button_one },
                CleanupAction::ReleaseKey {
                    key: key_two,
                    modifier: false,
                },
                CleanupAction::ReleaseKey {
                    key: key_one,
                    modifier: false,
                },
                CleanupAction::ReleaseKey {
                    key: modifier_two,
                    modifier: true,
                },
                CleanupAction::ReleaseKey {
                    key: modifier_one,
                    modifier: true,
                },
            ]
        );
        Ok(())
    }
}
