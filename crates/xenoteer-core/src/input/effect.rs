//! Bounded evidence for submitted and barrier-confirmed physical effects.

use thiserror::Error;

use crate::domain::RootPoint;

use super::{PhysicalButton, PhysicalKey};

/// Maximum effect records retained for one bounded input action plus cleanup.
///
/// Planned input is capped at 4,096 XTEST events. The additional 512 records
/// reserve enough space to release all 248 core keycodes and 255 core buttons
/// after a boundary failure without allowing cleanup evidence to be budgeted out.
pub const MAX_EFFECT_RECORDS: usize = 4_608;

/// A physical input side effect represented without backend details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The pointer was submitted toward an absolute point.
    PointerMoved {
        /// Requested absolute point.
        point: RootPoint,
    },
    /// A physical button press was submitted.
    ButtonPressed {
        /// Submitted button.
        button: PhysicalButton,
    },
    /// A physical button release was submitted.
    ButtonReleased {
        /// Submitted button.
        button: PhysicalButton,
    },
    /// A physical key press was submitted.
    KeyPressed {
        /// Submitted key.
        key: PhysicalKey,
        /// Whether the active XKB model classified the key as a modifier.
        modifier: bool,
    },
    /// A physical key release was submitted.
    KeyReleased {
        /// Submitted key.
        key: PhysicalKey,
        /// Whether the active XKB model classified the key as a modifier.
        modifier: bool,
    },
}

/// The strength of evidence for one submitted effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCertainty {
    /// The request was serialized but its checked cookie/barrier has not succeeded.
    Provisional,
    /// Checked requests and the same-connection barrier succeeded.
    Confirmed,
}

/// One ordered effect observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectRecord {
    sequence: u64,
    effect: Effect,
    certainty: EffectCertainty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectCheckpoint(usize);

impl EffectRecord {
    /// Returns the journal-local sequence number.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the represented side effect.
    #[must_use]
    pub const fn effect(self) -> Effect {
        self.effect
    }

    /// Returns the current evidence strength.
    #[must_use]
    pub const fn certainty(self) -> EffectCertainty {
        self.certainty
    }
}

/// A bounded append-only journal for one actor action.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectJournal {
    records: Vec<EffectRecord>,
    next_sequence: u64,
}

impl EffectJournal {
    /// Creates an empty effect journal.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
            next_sequence: 0,
        }
    }

    /// Appends an effect that has been serialized but not yet barrier-confirmed.
    pub(crate) fn record_provisional(&mut self, effect: Effect) -> Result<u64, EffectJournalError> {
        if self.records.len() >= MAX_EFFECT_RECORDS {
            return Err(EffectJournalError::CapacityExceeded);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(EffectJournalError::SequenceExhausted)?;
        self.records.push(EffectRecord {
            sequence,
            effect,
            certainty: EffectCertainty::Provisional,
        });
        Ok(sequence)
    }

    /// Captures the current end of the journal for batch-scoped confirmation.
    #[must_use]
    pub(crate) const fn checkpoint(&self) -> EffectCheckpoint {
        EffectCheckpoint(self.records.len())
    }

    /// Confirms effects appended since a valid checkpoint.
    pub(crate) fn confirm_since(
        &mut self,
        checkpoint: EffectCheckpoint,
    ) -> Result<(), EffectJournalError> {
        let records = self
            .records
            .get_mut(checkpoint.0..)
            .ok_or(EffectJournalError::InvalidCheckpoint)?;
        for record in records {
            record.certainty = EffectCertainty::Confirmed;
        }
        Ok(())
    }

    /// Returns all records in submission order.
    #[must_use]
    pub fn records(&self) -> &[EffectRecord] {
        &self.records
    }

    /// Returns whether any submitted effect remains uncertain.
    #[must_use]
    pub fn has_provisional(&self) -> bool {
        self.records
            .iter()
            .any(|record| record.certainty == EffectCertainty::Provisional)
    }

    /// Returns the number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns whether no effects have been submitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Failure to append bounded effect evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EffectJournalError {
    /// One action attempted to retain more effects than its global event bound.
    #[error("effect journal capacity exceeded")]
    CapacityExceeded,
    /// The journal-local sequence counter cannot advance.
    #[error("effect journal sequence exhausted")]
    SequenceExhausted,
    /// A checkpoint did not belong to the current journal prefix.
    #[error("effect journal checkpoint is invalid")]
    InvalidCheckpoint,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputActionError;

    #[test]
    fn effects_remain_provisional_until_explicit_confirmation()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut journal = EffectJournal::new();
        let checkpoint = journal.checkpoint();
        journal.record_provisional(Effect::ButtonPressed {
            button: PhysicalButton::new(1)?,
        })?;
        assert!(journal.has_provisional());
        assert_eq!(
            journal.records()[0].certainty(),
            EffectCertainty::Provisional
        );
        journal.confirm_since(checkpoint)?;
        assert!(!journal.has_provisional());
        assert_eq!(journal.records()[0].certainty(), EffectCertainty::Confirmed);
        Ok(())
    }

    #[test]
    fn physical_button_error_remains_structured() {
        assert_eq!(
            PhysicalButton::new(0),
            Err(InputActionError::InvalidPhysicalButton)
        );
    }
}
