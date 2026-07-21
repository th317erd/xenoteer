//! Validated, bounded keyboard requests resolved only inside the input actor.

use core::fmt;

use crate::keyboard::KeyIdentifier;

/// Maximum complete press/chord units in one sequence.
pub const MAX_KEYBOARD_SEQUENCE_STEPS: usize = 1_024;
/// Maximum Unicode scalar values in one physical text action.
pub const MAX_PHYSICAL_TEXT_SCALARS: usize = 128;
/// Maximum delay accepted on one keyboard transition boundary.
pub const MAX_KEYBOARD_DELAY_MS: u16 = 10_000;
/// Maximum sum of caller-requested keyboard delays in one action.
pub const MAX_KEYBOARD_TOTAL_DURATION_MS: u32 = 300_000;
/// Maximum distinct requested keys in one chord.
pub const MAX_KEYBOARD_CHORD_KEYS: usize = 16;
/// Hard bound shared with complete physical-input actions.
pub const MAX_KEYBOARD_ACTION_EVENTS: usize = 4_096;

/// Whether exact physical text may temporarily use a reserved unused keycode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalTextMode {
    /// Use only an exact current-layout binding and fail if none exists.
    CurrentLayout,
    /// Opt in to exact, per-scalar temporary mapping with verified restoration.
    ///
    /// # Global X11 safety requirement
    ///
    /// The caller or higher-level coordinator **must** prove that this bot has
    /// exclusive controller ownership and that server-side VNC/viewer input is
    /// disabled before selecting this mode. The actor serializes its own FIFO,
    /// but cannot exclude unrelated X clients from sending input or changing
    /// the global keymap while a temporary mapping is installed.
    ExtendedTemporaryMapping,
}

/// One complete press or chord in a cancellable sequence.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyboardSequenceStep {
    keys: Vec<KeyIdentifier>,
    hold_ms: u16,
    delay_before_ms: u16,
}

impl fmt::Debug for KeyboardSequenceStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyboardSequenceStep")
            .field("key_count", &self.keys.len())
            .field("hold_ms", &self.hold_ms)
            .field("delay_before_ms", &self.delay_before_ms)
            .finish()
    }
}

impl KeyboardSequenceStep {
    /// Builds one complete key press.
    pub fn press(
        key: KeyIdentifier,
        hold_ms: u16,
        delay_before_ms: u16,
    ) -> Result<Self, KeyboardActionError> {
        Self::chord(&[key], hold_ms, delay_before_ms)
    }

    /// Builds one complete modifier-first chord.
    pub fn chord(
        keys: &[KeyIdentifier],
        hold_ms: u16,
        delay_before_ms: u16,
    ) -> Result<Self, KeyboardActionError> {
        validate_keys(keys)?;
        validate_delay(hold_ms)?;
        validate_delay(delay_before_ms)?;
        Ok(Self {
            keys: keys.to_vec(),
            hold_ms,
            delay_before_ms,
        })
    }

    /// Requested identifiers in caller order.
    #[must_use]
    pub fn keys(&self) -> &[KeyIdentifier] {
        &self.keys
    }

    /// Time between the final press and first release.
    #[must_use]
    pub const fn hold_ms(&self) -> u16 {
        self.hold_ms
    }

    /// Delay attached to the first event in this complete unit.
    #[must_use]
    pub const fn delay_before_ms(&self) -> u16 {
        self.delay_before_ms
    }

    fn event_upper_bound(&self) -> usize {
        self.keys.len().saturating_add(8).saturating_mul(2)
    }
}

/// One bounded unresolved keyboard action admitted to the actor FIFO.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyboardAction {
    pub(super) kind: KeyboardActionKind,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum KeyboardActionKind {
    Down(KeyIdentifier),
    Up(KeyIdentifier),
    Sequence(Vec<KeyboardSequenceStep>),
    Text {
        text: String,
        mode: PhysicalTextMode,
        inter_character_delay_ms: u16,
    },
}

impl KeyboardAction {
    /// Captures and holds one actor-resolved physical binding.
    pub fn down(key: KeyIdentifier) -> Result<Self, KeyboardActionError> {
        validate_identifier(key)?;
        Ok(Self {
            kind: KeyboardActionKind::Down(key),
        })
    }

    /// Releases the exact binding captured by a prior matching down action.
    pub fn up(key: KeyIdentifier) -> Result<Self, KeyboardActionError> {
        validate_identifier(key)?;
        Ok(Self {
            kind: KeyboardActionKind::Up(key),
        })
    }

    /// Presses and releases one resolved key as one complete unit.
    pub fn press(key: KeyIdentifier, hold_ms: u16) -> Result<Self, KeyboardActionError> {
        Self::sequence(&[KeyboardSequenceStep::press(key, hold_ms, 0)?])
    }

    /// Presses modifiers first, then other keys, and releases in exact reverse order.
    pub fn chord(keys: &[KeyIdentifier], hold_ms: u16) -> Result<Self, KeyboardActionError> {
        Self::sequence(&[KeyboardSequenceStep::chord(keys, hold_ms, 0)?])
    }

    /// Executes complete steps with cancellation only between steps.
    pub fn sequence(steps: &[KeyboardSequenceStep]) -> Result<Self, KeyboardActionError> {
        if steps.is_empty() {
            return Err(KeyboardActionError::EmptySequence);
        }
        if steps.len() > MAX_KEYBOARD_SEQUENCE_STEPS {
            return Err(KeyboardActionError::TooManySequenceSteps {
                actual: steps.len(),
            });
        }
        let total_duration_ms = steps.iter().try_fold(0_u32, |total, step| {
            total
                .checked_add(u32::from(step.hold_ms))
                .and_then(|value| value.checked_add(u32::from(step.delay_before_ms)))
                .ok_or(KeyboardActionError::DurationOverflow)
        })?;
        if total_duration_ms > MAX_KEYBOARD_TOTAL_DURATION_MS {
            return Err(KeyboardActionError::TotalDurationTooLong {
                actual_ms: total_duration_ms,
            });
        }
        let events = steps.iter().fold(0_usize, |total, step| {
            total.saturating_add(step.event_upper_bound())
        });
        if events > MAX_KEYBOARD_ACTION_EVENTS {
            return Err(KeyboardActionError::TooManyEvents { actual: events });
        }
        Ok(Self {
            kind: KeyboardActionKind::Sequence(steps.to_vec()),
        })
    }

    /// Types exact Unicode scalars through physical key events.
    pub fn physical_text(
        text: impl Into<String>,
        mode: PhysicalTextMode,
        inter_character_delay_ms: u16,
    ) -> Result<Self, KeyboardActionError> {
        validate_delay(inter_character_delay_ms)?;
        let text = text.into();
        let scalar_count = text.chars().count();
        if scalar_count == 0 {
            return Err(KeyboardActionError::EmptyText);
        }
        if scalar_count > MAX_PHYSICAL_TEXT_SCALARS {
            return Err(KeyboardActionError::TooManyTextScalars {
                actual: scalar_count,
            });
        }
        let scalar_gaps = u32::try_from(scalar_count.saturating_sub(1))
            .map_err(|_| KeyboardActionError::DurationOverflow)?;
        let total_duration_ms = u32::from(inter_character_delay_ms).saturating_mul(scalar_gaps);
        if total_duration_ms > MAX_KEYBOARD_TOTAL_DURATION_MS {
            return Err(KeyboardActionError::TotalDurationTooLong {
                actual_ms: total_duration_ms,
            });
        }
        let events = scalar_count.saturating_mul(18);
        if events > MAX_KEYBOARD_ACTION_EVENTS {
            return Err(KeyboardActionError::TooManyEvents { actual: events });
        }
        Ok(Self {
            kind: KeyboardActionKind::Text {
                text,
                mode,
                inter_character_delay_ms,
            },
        })
    }

    /// Conservative complete-event upper bound before actor-side resolution.
    #[must_use]
    pub fn event_upper_bound(&self) -> usize {
        match &self.kind {
            KeyboardActionKind::Down(_) | KeyboardActionKind::Up(_) => 9,
            KeyboardActionKind::Sequence(steps) => steps.iter().fold(0_usize, |total, step| {
                total.saturating_add(step.event_upper_bound())
            }),
            KeyboardActionKind::Text { text, .. } => text.chars().count().saturating_mul(18),
        }
    }

    pub(super) fn contains_scalar_identifier(&self) -> bool {
        match &self.kind {
            KeyboardActionKind::Down(KeyIdentifier::Scalar(_))
            | KeyboardActionKind::Up(KeyIdentifier::Scalar(_))
            | KeyboardActionKind::Text { .. } => true,
            KeyboardActionKind::Sequence(steps) => steps.iter().any(|step| {
                step.keys
                    .iter()
                    .any(|key| matches!(key, KeyIdentifier::Scalar(_)))
            }),
            KeyboardActionKind::Down(_) | KeyboardActionKind::Up(_) => false,
        }
    }
}

impl fmt::Debug for KeyboardAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            KeyboardActionKind::Down(key) => formatter
                .debug_struct("Down")
                .field("identifier_kind", &identifier_kind(*key))
                .finish(),
            KeyboardActionKind::Up(key) => formatter
                .debug_struct("Up")
                .field("identifier_kind", &identifier_kind(*key))
                .finish(),
            KeyboardActionKind::Sequence(steps) => {
                formatter.debug_tuple("Sequence").field(steps).finish()
            }
            KeyboardActionKind::Text { text, mode, .. } => formatter
                .debug_struct("PhysicalText")
                .field("scalar_count", &text.chars().count())
                .field("mode", mode)
                .finish_non_exhaustive(),
        }
    }
}

/// Why an unresolved keyboard action was rejected before admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyboardActionError {
    /// A raw keycode is below the core protocol minimum.
    #[error("raw keycode {keycode} is below the core X11 minimum of 8")]
    InvalidRawKeycode {
        /// Rejected raw keycode.
        keycode: u8,
    },
    /// A chord must contain at least one key.
    #[error("keyboard chord must not be empty")]
    EmptyChord,
    /// A chord exceeds the bounded distinct-key count.
    #[error("keyboard chord has {actual} keys; maximum is 16")]
    TooManyChordKeys {
        /// Requested count.
        actual: usize,
    },
    /// A chord repeats one requested identity.
    #[error("keyboard chord repeats a key identity")]
    DuplicateChordKey,
    /// A sequence must contain at least one complete unit.
    #[error("keyboard sequence must not be empty")]
    EmptySequence,
    /// A sequence exceeds the complete-unit bound.
    #[error("keyboard sequence has {actual} steps; maximum is 1024")]
    TooManySequenceSteps {
        /// Requested count.
        actual: usize,
    },
    /// Exact physical text must contain at least one scalar.
    #[error("physical text must not be empty")]
    EmptyText,
    /// Exact physical text exceeds the scalar bound.
    #[error("physical text has {actual} scalars; maximum is 128")]
    TooManyTextScalars {
        /// Requested scalar count.
        actual: usize,
    },
    /// One delay exceeds the XTEST request-delay bound.
    #[error("keyboard delay {actual_ms} ms exceeds 10000 ms")]
    DelayTooLong {
        /// Rejected delay.
        actual_ms: u16,
    },
    /// Delay summation overflowed.
    #[error("keyboard action duration overflowed")]
    DurationOverflow,
    /// Total requested delay exceeds the action bound.
    #[error("keyboard action duration {actual_ms} ms exceeds 300000 ms")]
    TotalDurationTooLong {
        /// Rejected total.
        actual_ms: u32,
    },
    /// Conservative resolved-event bound exceeds the complete-action cap.
    #[error("keyboard action may emit {actual} events; maximum is 4096")]
    TooManyEvents {
        /// Conservative event count.
        actual: usize,
    },
}

fn validate_keys(keys: &[KeyIdentifier]) -> Result<(), KeyboardActionError> {
    if keys.is_empty() {
        return Err(KeyboardActionError::EmptyChord);
    }
    if keys.len() > MAX_KEYBOARD_CHORD_KEYS {
        return Err(KeyboardActionError::TooManyChordKeys { actual: keys.len() });
    }
    for (index, key) in keys.iter().copied().enumerate() {
        validate_identifier(key)?;
        if keys[..index].contains(&key) {
            return Err(KeyboardActionError::DuplicateChordKey);
        }
    }
    Ok(())
}

fn validate_identifier(key: KeyIdentifier) -> Result<(), KeyboardActionError> {
    if let KeyIdentifier::Raw(keycode @ 0..=7) = key {
        Err(KeyboardActionError::InvalidRawKeycode { keycode })
    } else {
        Ok(())
    }
}

fn validate_delay(delay_ms: u16) -> Result<(), KeyboardActionError> {
    if delay_ms > MAX_KEYBOARD_DELAY_MS {
        Err(KeyboardActionError::DelayTooLong {
            actual_ms: delay_ms,
        })
    } else {
        Ok(())
    }
}

const fn identifier_kind(identifier: KeyIdentifier) -> &'static str {
    match identifier {
        KeyIdentifier::Named(_) => "named",
        KeyIdentifier::Scalar(_) => "scalar",
        KeyIdentifier::Raw(_) => "raw",
    }
}
