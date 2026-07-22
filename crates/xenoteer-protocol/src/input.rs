//! Strict, backend-independent compound physical-input command shapes.

use core::fmt;
use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::{StrictPoint, deserialize_strict_point};
use crate::window::{StrictWindowRef, deserialize_strict_window_ref};
use crate::{Point, WindowRef, WindowValidationError};

/// Maximum duration carried by one pointer motion primitive.
pub const MAX_POINTER_MOVE_DURATION_MS: u32 = 10_000;
/// Maximum click count in one atomic command.
pub const MAX_POINTER_CLICK_COUNT: u8 = 5;
/// Maximum discrete scroll notches in one atomic command.
pub const MAX_POINTER_SCROLL_COUNT: u16 = 1_000;
/// Maximum delay between discrete scroll notches.
pub const MAX_POINTER_SCROLL_INTERVAL_MS: u16 = 1_000;
/// Fixed threshold used to validate an atomic multi-click interval.
pub const DEFAULT_DOUBLE_CLICK_THRESHOLD_MS: u16 = 250;
/// Maximum complete press/chord units in one keyboard sequence.
pub const MAX_KEYBOARD_SEQUENCE_STEPS: usize = 1_024;
/// Maximum distinct requested identities in one chord.
pub const MAX_KEYBOARD_CHORD_KEYS: usize = 16;
/// Maximum delay on one keyboard transition boundary.
pub const MAX_KEYBOARD_DELAY_MS: u16 = 10_000;
/// Maximum sum of caller-requested keyboard delays in one action.
pub const MAX_KEYBOARD_TOTAL_DURATION_MS: u32 = 300_000;
/// Conservative maximum actor-side keyboard events in one action.
pub const MAX_KEYBOARD_ACTION_EVENTS: usize = 4_096;

/// Interpolation curve for a pointer movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerCurve {
    /// Move directly to the endpoint with no intermediate samples.
    Instant,
    /// Use equally spaced samples.
    Linear,
    /// Use an ease-in/ease-out smoothstep curve.
    Smooth,
}

/// A global-pointer movement relative to its execution-time position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerMoveRelativeCommand {
    /// Signed root-physical displacement from the live pointer position.
    #[serde(deserialize_with = "deserialize_strict_point")]
    #[schemars(with = "StrictPoint")]
    pub delta: Point,
    /// Requested whole-path duration. Omission selects the configured default.
    #[schemars(range(max = MAX_POINTER_MOVE_DURATION_MS))]
    pub duration_ms: Option<u32>,
    /// Interpolation curve.
    pub curve: PointerCurve,
}

impl PointerMoveRelativeCommand {
    /// Validates pointer timing invariants.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        validate_pointer_motion(self.duration_ms, self.curve)
    }
}

/// Stable logical buttons accepted by complete click and drag commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerLogicalButton {
    /// Primary button.
    Left,
    /// Middle button.
    Middle,
    /// Secondary button.
    Right,
    /// Browser-style back button.
    Back,
    /// Browser-style forward button.
    Forward,
}

/// Coordinate frame for a point local to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowPointerCoordinateSpace {
    /// Coordinates relative to the client content window.
    Client,
    /// Coordinates relative to the window-manager frame.
    Frame,
}

/// Policy for a window-local point outside the selected client or frame rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowPointerBoundsPolicy {
    /// Reject a point outside the selected rectangle.
    Reject,
    /// Clamp each coordinate to the nearest point inside the selected rectangle.
    Clamp,
    /// Permit a translated point outside the rectangle when it still fits root coordinates.
    Allow,
}

/// Optional motion destination resolved immediately before an atomic click.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PointerClickTarget {
    /// Click at the pointer's execution-time position.
    Current,
    /// Move to an absolute root-physical point before clicking.
    Root {
        /// Absolute root-physical endpoint.
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        point: Point,
    },
    /// Resolve a window-local point against live geometry before clicking.
    Window {
        /// Exact observed window birth.
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
        /// Whether `point` is client-relative or frame-relative.
        coordinate_space: WindowPointerCoordinateSpace,
        /// Point in the selected window coordinate space.
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        point: Point,
        /// Ask the window manager to activate the exact window before input.
        activate: bool,
        /// Handling for a point outside the selected live rectangle.
        bounds_policy: WindowPointerBoundsPolicy,
    },
}

impl PointerClickTarget {
    /// Exact window reference carried by a targeted click, if any.
    #[must_use]
    pub const fn window(&self) -> Option<&WindowRef> {
        match self {
            Self::Window { window, .. } => Some(window),
            Self::Current | Self::Root { .. } => None,
        }
    }

    fn validate(&self) -> Result<(), InputValidationError> {
        if let Self::Window { window, .. } = self {
            window.validate_shape()?;
        }
        Ok(())
    }
}

/// One bounded, FIFO-atomic click sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerClickCommand {
    /// Current, absolute-root, or exact-window click destination.
    pub target: PointerClickTarget,
    /// Logical button resolved from the live X11 pointer mapping.
    pub button: PointerLogicalButton,
    /// Number of complete press/release clicks.
    #[schemars(range(min = 1, max = MAX_POINTER_CLICK_COUNT))]
    pub count: u8,
    /// Requested whole-path motion duration when the target requires movement.
    #[schemars(range(max = MAX_POINTER_MOVE_DURATION_MS))]
    pub duration_ms: Option<u32>,
    /// Motion interpolation curve.
    pub curve: PointerCurve,
    /// Dwell attached to the first button press.
    #[schemars(range(max = 10_000))]
    pub pre_click_dwell_ms: u16,
    /// Delay between each press and release.
    #[schemars(range(max = 10_000))]
    pub press_duration_ms: u16,
    /// Delay between complete clicks; must remain below 250 ms.
    #[schemars(range(max = 249))]
    pub inter_click_interval_ms: u16,
}

impl PointerClickCommand {
    /// Validates bounded motion, timing, target, and click-count invariants.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        self.target.validate()?;
        if !(1..=MAX_POINTER_CLICK_COUNT).contains(&self.count) {
            return Err(InputValidationError::InvalidClickCount);
        }
        validate_pointer_motion(self.duration_ms, self.curve)?;
        validate_xtest_delay(self.pre_click_dwell_ms)?;
        validate_xtest_delay(self.press_duration_ms)?;
        if self.inter_click_interval_ms >= DEFAULT_DOUBLE_CLICK_THRESHOLD_MS {
            return Err(InputValidationError::InterClickIntervalTooLong);
        }
        Ok(())
    }
}

/// Execution-time destination for an atomic drag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PointerDragTarget {
    /// Absolute root-physical endpoint.
    Root {
        /// Absolute endpoint.
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        point: Point,
    },
    /// Signed displacement from the execution-time pointer position.
    Relative {
        /// Relative root-physical displacement.
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        delta: Point,
    },
}

/// One bounded, FIFO-atomic press/move/release drag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerDragCommand {
    /// Root or execution-time-relative drag endpoint.
    pub target: PointerDragTarget,
    /// Logical button held for the drag.
    pub button: PointerLogicalButton,
    /// Requested whole-path duration.
    #[schemars(range(max = MAX_POINTER_MOVE_DURATION_MS))]
    pub duration_ms: Option<u32>,
    /// Motion interpolation curve.
    pub curve: PointerCurve,
    /// Delay after the press and before motion.
    #[schemars(range(max = 10_000))]
    pub press_dwell_ms: u16,
    /// Delay on the release boundary.
    #[schemars(range(max = 10_000))]
    pub release_dwell_ms: u16,
}

impl PointerDragCommand {
    /// Validates bounded motion and dwell timing.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        validate_pointer_motion(self.duration_ms, self.curve)?;
        validate_xtest_delay(self.press_dwell_ms)?;
        validate_xtest_delay(self.release_dwell_ms)
    }
}

/// Direction of discrete logical scroll notches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerScrollDirection {
    /// Vertical scroll toward the top.
    Up,
    /// Vertical scroll toward the bottom.
    Down,
    /// Horizontal scroll toward the left.
    Left,
    /// Horizontal scroll toward the right.
    Right,
}

/// One bounded, FIFO-atomic discrete scroll sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerScrollCommand {
    /// Logical scroll direction.
    pub direction: PointerScrollDirection,
    /// Number of complete press/release notches.
    #[schemars(range(min = 1, max = MAX_POINTER_SCROLL_COUNT))]
    pub count: u16,
    /// Delay between complete notches.
    #[schemars(range(max = MAX_POINTER_SCROLL_INTERVAL_MS))]
    pub interval_ms: u16,
}

impl PointerScrollCommand {
    /// Validates notch count and interval bounds.
    pub fn validate(self) -> Result<(), InputValidationError> {
        if !(1..=MAX_POINTER_SCROLL_COUNT).contains(&self.count) {
            return Err(InputValidationError::InvalidScrollCount);
        }
        if self.interval_ms > MAX_POINTER_SCROLL_INTERVAL_MS {
            return Err(InputValidationError::ScrollIntervalTooLong);
        }
        Ok(())
    }
}

/// Closed, versioned named-key vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardNamedKey {
    /// Backspace.
    Backspace,
    /// Horizontal tab.
    Tab,
    /// Return/enter.
    Enter,
    /// Escape.
    Escape,
    /// Space.
    Space,
    /// Insert.
    Insert,
    /// Delete.
    Delete,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Left arrow.
    ArrowLeft,
    /// Up arrow.
    ArrowUp,
    /// Right arrow.
    ArrowRight,
    /// Down arrow.
    ArrowDown,
    /// Configured default shift side.
    Shift,
    /// Configured default control side.
    Control,
    /// Configured default alt side.
    Alt,
    /// Configured default meta side.
    Meta,
    /// Configured default super side.
    Super,
    /// Left shift.
    ShiftLeft,
    /// Right shift.
    ShiftRight,
    /// Left control.
    ControlLeft,
    /// Right control.
    ControlRight,
    /// Left alt.
    AltLeft,
    /// Right alt.
    AltRight,
    /// Left meta.
    MetaLeft,
    /// Right meta.
    MetaRight,
    /// Left super.
    SuperLeft,
    /// Right super.
    SuperRight,
    /// Left hyper.
    HyperLeft,
    /// Right hyper.
    HyperRight,
    /// ISO level-three shift, normally AltGr.
    AltGraph,
    /// Caps lock.
    CapsLock,
    /// Number lock.
    NumLock,
    /// Scroll lock.
    ScrollLock,
    /// Print screen.
    PrintScreen,
    /// Pause/break.
    Pause,
    /// Context menu.
    ContextMenu,
    /// Function key 1.
    F1,
    /// Function key 2.
    F2,
    /// Function key 3.
    F3,
    /// Function key 4.
    F4,
    /// Function key 5.
    F5,
    /// Function key 6.
    F6,
    /// Function key 7.
    F7,
    /// Function key 8.
    F8,
    /// Function key 9.
    F9,
    /// Function key 10.
    F10,
    /// Function key 11.
    F11,
    /// Function key 12.
    F12,
    /// Function key 13.
    F13,
    /// Function key 14.
    F14,
    /// Function key 15.
    F15,
    /// Function key 16.
    F16,
    /// Function key 17.
    F17,
    /// Function key 18.
    F18,
    /// Function key 19.
    F19,
    /// Function key 20.
    F20,
    /// Function key 21.
    F21,
    /// Function key 22.
    F22,
    /// Function key 23.
    F23,
    /// Function key 24.
    F24,
}

/// Named, Unicode-scalar, or advanced raw physical key identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeyboardKeyIdentifier {
    /// One member of the closed named-key vocabulary.
    Named {
        /// Stable named-key value.
        name: KeyboardNamedKey,
    },
    /// Exactly one non-control Unicode scalar.
    Scalar {
        /// Requested Unicode scalar; diagnostic formatting always redacts it.
        value: char,
    },
    /// Advanced core-X11 physical keycode escape hatch.
    Raw {
        /// Physical core-X11 keycode.
        #[schemars(range(min = 8))]
        keycode: u8,
    },
}

impl fmt::Debug for KeyboardKeyIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named { name } => formatter.debug_tuple("Named").field(name).finish(),
            Self::Scalar { .. } => formatter.write_str("Scalar(<redacted>)"),
            Self::Raw { keycode } => formatter.debug_tuple("Raw").field(keycode).finish(),
        }
    }
}

impl KeyboardKeyIdentifier {
    /// Validates scalar and raw-key bounds.
    pub fn validate(self) -> Result<(), InputValidationError> {
        match self {
            Self::Scalar { value } if value.is_control() => {
                Err(InputValidationError::ControlScalar)
            }
            Self::Raw { keycode } if keycode < 8 => Err(InputValidationError::InvalidRawKeycode),
            Self::Named { .. } | Self::Scalar { .. } | Self::Raw { .. } => Ok(()),
        }
    }
}

/// One complete press or chord in an atomic keyboard sequence.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardSequenceStep {
    /// Modifier-first key identities, released in reverse order.
    #[schemars(length(min = 1, max = MAX_KEYBOARD_CHORD_KEYS))]
    pub keys: Vec<KeyboardKeyIdentifier>,
    /// Delay before this complete step begins.
    #[schemars(range(max = MAX_KEYBOARD_DELAY_MS))]
    pub delay_before_ms: u16,
    /// Delay between final press and first release.
    #[schemars(range(max = MAX_KEYBOARD_DELAY_MS))]
    pub hold_ms: u16,
}

impl fmt::Debug for KeyboardSequenceStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyboardSequenceStep")
            .field("key_count", &self.keys.len())
            .field("delay_before_ms", &self.delay_before_ms)
            .field("hold_ms", &self.hold_ms)
            .finish()
    }
}

impl KeyboardSequenceStep {
    fn validate(&self) -> Result<(), InputValidationError> {
        validate_keys(&self.keys)?;
        validate_keyboard_delay(self.delay_before_ms)?;
        validate_keyboard_delay(self.hold_ms)
    }

    fn event_upper_bound(&self) -> usize {
        self.keys.len().saturating_add(8).saturating_mul(2)
    }
}

/// One complete named/scalar/raw key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardPressCommand {
    /// Unresolved key identity.
    pub key: KeyboardKeyIdentifier,
    /// Delay between press and release.
    #[schemars(range(max = MAX_KEYBOARD_DELAY_MS))]
    pub hold_ms: u16,
}

impl KeyboardPressCommand {
    /// Validates the identity and hold duration.
    pub fn validate(self) -> Result<(), InputValidationError> {
        self.key.validate()?;
        validate_keyboard_delay(self.hold_ms)
    }
}

/// One complete modifier-first chord, released in reverse order.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardChordCommand {
    /// Ordered unresolved identities.
    #[schemars(length(min = 1, max = MAX_KEYBOARD_CHORD_KEYS))]
    pub keys: Vec<KeyboardKeyIdentifier>,
    /// Delay between final press and first release.
    #[schemars(range(max = MAX_KEYBOARD_DELAY_MS))]
    pub hold_ms: u16,
}

impl fmt::Debug for KeyboardChordCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyboardChordCommand")
            .field("key_count", &self.keys.len())
            .field("hold_ms", &self.hold_ms)
            .finish()
    }
}

impl KeyboardChordCommand {
    /// Validates key uniqueness/count and hold duration.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        validate_keys(&self.keys)?;
        validate_keyboard_delay(self.hold_ms)
    }
}

/// A bounded sequence of complete key presses/chords executed without FIFO interleaving.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardSequenceCommand {
    /// Ordered complete keyboard units.
    #[schemars(length(min = 1, max = MAX_KEYBOARD_SEQUENCE_STEPS))]
    pub steps: Vec<KeyboardSequenceStep>,
}

impl fmt::Debug for KeyboardSequenceCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyboardSequenceCommand")
            .field("step_count", &self.steps.len())
            .finish()
    }
}

impl KeyboardSequenceCommand {
    /// Validates per-step and aggregate duration/event bounds.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        if self.steps.is_empty() || self.steps.len() > MAX_KEYBOARD_SEQUENCE_STEPS {
            return Err(InputValidationError::InvalidSequenceLength);
        }
        let mut total_duration_ms = 0_u32;
        let mut total_events = 0_usize;
        for step in &self.steps {
            step.validate()?;
            total_duration_ms = total_duration_ms
                .checked_add(u32::from(step.delay_before_ms))
                .and_then(|value| value.checked_add(u32::from(step.hold_ms)))
                .ok_or(InputValidationError::KeyboardDurationOverflow)?;
            total_events = total_events
                .checked_add(step.event_upper_bound())
                .ok_or(InputValidationError::TooManyKeyboardEvents)?;
        }
        if total_duration_ms > MAX_KEYBOARD_TOTAL_DURATION_MS {
            return Err(InputValidationError::KeyboardTotalDurationTooLong);
        }
        if total_events > MAX_KEYBOARD_ACTION_EVENTS {
            return Err(InputValidationError::TooManyKeyboardEvents);
        }
        Ok(())
    }
}

fn validate_pointer_motion(
    duration_ms: Option<u32>,
    curve: PointerCurve,
) -> Result<(), InputValidationError> {
    if duration_ms.is_some_and(|duration| duration > MAX_POINTER_MOVE_DURATION_MS) {
        return Err(InputValidationError::PointerDurationTooLong);
    }
    if curve == PointerCurve::Instant && duration_ms.is_some_and(|duration| duration != 0) {
        return Err(InputValidationError::InstantPointerDuration);
    }
    Ok(())
}

fn validate_xtest_delay(delay_ms: u16) -> Result<(), InputValidationError> {
    if u32::from(delay_ms) > MAX_POINTER_MOVE_DURATION_MS {
        Err(InputValidationError::InputDelayTooLong)
    } else {
        Ok(())
    }
}

fn validate_keyboard_delay(delay_ms: u16) -> Result<(), InputValidationError> {
    if delay_ms > MAX_KEYBOARD_DELAY_MS {
        Err(InputValidationError::KeyboardDelayTooLong)
    } else {
        Ok(())
    }
}

fn validate_keys(keys: &[KeyboardKeyIdentifier]) -> Result<(), InputValidationError> {
    if keys.is_empty() || keys.len() > MAX_KEYBOARD_CHORD_KEYS {
        return Err(InputValidationError::InvalidChordLength);
    }
    let mut unique = HashSet::with_capacity(keys.len());
    for key in keys {
        key.validate()?;
        if !unique.insert(*key) {
            return Err(InputValidationError::DuplicateChordKey);
        }
    }
    Ok(())
}

/// Strict public-input validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputValidationError {
    /// A pointer duration exceeds the public bound.
    #[error("pointer motion duration exceeds protocol maximum")]
    PointerDurationTooLong,
    /// Instant motion carried a non-zero duration.
    #[error("instant pointer motion requires an omitted or zero duration")]
    InstantPointerDuration,
    /// A click count was zero or above the atomic maximum.
    #[error("click count must be between one and five")]
    InvalidClickCount,
    /// A pointer delay exceeded the XTEST request bound.
    #[error("one XTEST delay exceeds the protocol maximum")]
    InputDelayTooLong,
    /// A multi-click interval reached or exceeded the fixed threshold.
    #[error("multi-click interval must remain below the double-click threshold")]
    InterClickIntervalTooLong,
    /// A discrete scroll count was zero or above the atomic maximum.
    #[error("scroll count must be between one and one thousand")]
    InvalidScrollCount,
    /// A discrete scroll interval exceeded its public bound.
    #[error("scroll interval exceeds the protocol maximum")]
    ScrollIntervalTooLong,
    /// A raw keycode used X11's reserved low range.
    #[error("raw keycode is below the core X11 minimum")]
    InvalidRawKeycode,
    /// A scalar key identifier was a control character.
    #[error("control Unicode scalars are not accepted as physical key identifiers")]
    ControlScalar,
    /// A chord was empty or exceeded the distinct-key maximum.
    #[error("keyboard chord must contain between one and sixteen distinct keys")]
    InvalidChordLength,
    /// A chord contained the same unresolved identity more than once.
    #[error("keyboard chord repeats a key identity")]
    DuplicateChordKey,
    /// One keyboard boundary delay exceeded its public bound.
    #[error("keyboard delay exceeds the protocol maximum")]
    KeyboardDelayTooLong,
    /// A keyboard sequence was empty or contained too many steps.
    #[error("keyboard sequence must contain between one and 1024 steps")]
    InvalidSequenceLength,
    /// Aggregate keyboard duration arithmetic overflowed.
    #[error("keyboard sequence duration overflowed")]
    KeyboardDurationOverflow,
    /// Aggregate keyboard duration exceeded its public bound.
    #[error("keyboard sequence duration exceeds the protocol maximum")]
    KeyboardTotalDurationTooLong,
    /// Conservative actor-side event expansion exceeded its bound.
    #[error("keyboard action exceeds the conservative event maximum")]
    TooManyKeyboardEvents,
    /// The exact window reference failed shape validation.
    #[error("window reference is invalid: {0}")]
    Window(#[from] WindowValidationError),
}
