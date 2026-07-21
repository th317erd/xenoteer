//! Deterministic physical-key chord planning after XKB resolution.

use thiserror::Error;

use super::PhysicalKey;

/// Maximum distinct resolved physical keys in one atomic chord.
pub const MAX_CHORD_KEYS: usize = 16;

/// A physical key plus its classification in the current XKB model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedKey {
    key: PhysicalKey,
    modifier: bool,
}

impl ResolvedKey {
    /// Creates one XKB-resolved key.
    #[must_use]
    pub const fn new(key: PhysicalKey, modifier: bool) -> Self {
        Self { key, modifier }
    }

    /// Returns the physical X11 keycode.
    #[must_use]
    pub const fn key(self) -> PhysicalKey {
        self.key
    }

    /// Returns whether XKB classifies this key as a modifier.
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        self.modifier
    }
}

/// The direction of one planned physical key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    /// Press the key.
    Press,
    /// Release the key.
    Release,
}

/// One ordered physical key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    resolved: ResolvedKey,
    kind: KeyEventKind,
}

impl KeyEvent {
    /// Returns the physical key.
    #[must_use]
    pub const fn key(self) -> PhysicalKey {
        self.resolved.key()
    }

    /// Returns the resolved key with modifier classification.
    #[must_use]
    pub const fn resolved(self) -> ResolvedKey {
        self.resolved
    }

    /// Returns the transition direction.
    #[must_use]
    pub const fn kind(self) -> KeyEventKind {
        self.kind
    }

    /// Returns whether XKB classified this key as a modifier.
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        self.resolved.is_modifier()
    }
}

/// A balanced, atomic chord of distinct resolved physical keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPlan {
    caller_order: Vec<ResolvedKey>,
    press_order: Vec<ResolvedKey>,
    events: Vec<KeyEvent>,
}

impl KeyPlan {
    /// Plans a non-empty chord of up to sixteen distinct resolved keys.
    ///
    /// Modifier presses retain caller order and precede non-modifier presses,
    /// which also retain caller order. All releases reverse the complete press
    /// order so that modifiers are necessarily released last.
    pub fn chord(keys: &[ResolvedKey]) -> Result<Self, KeyPlanError> {
        if keys.is_empty() {
            return Err(KeyPlanError::EmptyChord);
        }
        if keys.len() > MAX_CHORD_KEYS {
            return Err(KeyPlanError::TooManyKeys { actual: keys.len() });
        }
        let mut seen = Vec::with_capacity(keys.len());
        for resolved in keys {
            if seen.contains(&resolved.key()) {
                return Err(KeyPlanError::DuplicateKey {
                    key: resolved.key(),
                });
            }
            seen.push(resolved.key());
        }

        let mut press_order = Vec::with_capacity(keys.len());
        press_order.extend(keys.iter().copied().filter(|key| key.is_modifier()));
        press_order.extend(keys.iter().copied().filter(|key| !key.is_modifier()));

        let mut events = Vec::with_capacity(press_order.len().saturating_mul(2));
        events.extend(press_order.iter().copied().map(|resolved| KeyEvent {
            resolved,
            kind: KeyEventKind::Press,
        }));
        events.extend(press_order.iter().rev().copied().map(|resolved| KeyEvent {
            resolved,
            kind: KeyEventKind::Release,
        }));
        Ok(Self {
            caller_order: keys.to_vec(),
            press_order,
            events,
        })
    }

    /// Plans one balanced physical key press.
    pub fn key(key: ResolvedKey) -> Result<Self, KeyPlanError> {
        Self::chord(&[key])
    }

    /// Returns resolved keys in caller order.
    #[must_use]
    pub fn caller_order(&self) -> &[ResolvedKey] {
        &self.caller_order
    }

    /// Returns the stable-partitioned key press order.
    #[must_use]
    pub fn press_order(&self) -> &[ResolvedKey] {
        &self.press_order
    }

    /// Returns all balanced transitions in send order.
    #[must_use]
    pub fn events(&self) -> &[KeyEvent] {
        &self.events
    }
}

/// Failure to construct an unambiguous physical key plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum KeyPlanError {
    /// A chord must contain at least one key.
    #[error("key chord must not be empty")]
    EmptyChord,
    /// The chord contains more than sixteen physical keys.
    #[error("key chord count {actual} exceeds {MAX_CHORD_KEYS}")]
    TooManyKeys {
        /// Rejected count.
        actual: usize,
    },
    /// One physical key appears more than once.
    #[error("physical key {key:?} appears more than once")]
    DuplicateKey {
        /// Repeated physical key.
        key: PhysicalKey,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_stably_partitions_and_reverses_complete_press_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let key_a = ResolvedKey::new(PhysicalKey::new(38)?, false);
        let control = ResolvedKey::new(PhysicalKey::new(37)?, true);
        let key_b = ResolvedKey::new(PhysicalKey::new(56)?, false);
        let shift = ResolvedKey::new(PhysicalKey::new(50)?, true);
        let plan = KeyPlan::chord(&[key_a, control, key_b, shift])?;
        assert_eq!(plan.press_order(), &[control, shift, key_a, key_b]);
        let transitions: Vec<(u8, KeyEventKind)> = plan
            .events()
            .iter()
            .map(|event| (event.key().keycode(), event.kind()))
            .collect();
        assert_eq!(
            transitions,
            vec![
                (37, KeyEventKind::Press),
                (50, KeyEventKind::Press),
                (38, KeyEventKind::Press),
                (56, KeyEventKind::Press),
                (56, KeyEventKind::Release),
                (38, KeyEventKind::Release),
                (50, KeyEventKind::Release),
                (37, KeyEventKind::Release),
            ]
        );
        Ok(())
    }

    #[test]
    fn empty_and_duplicate_chords_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(KeyPlan::chord(&[]), Err(KeyPlanError::EmptyChord));
        let key = ResolvedKey::new(PhysicalKey::new(38)?, false);
        assert_eq!(
            KeyPlan::chord(&[key, ResolvedKey::new(key.key(), true)]),
            Err(KeyPlanError::DuplicateKey { key: key.key() })
        );
        Ok(())
    }
}
