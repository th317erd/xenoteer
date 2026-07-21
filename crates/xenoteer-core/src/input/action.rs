//! Validated, backend-independent input actions and raw physical identifiers.

use thiserror::Error;

use super::MAX_MOTION_EVENTS;
use super::{KeyPlan, MotionPlan};

/// Smallest core X11 keycode.
pub const MIN_PHYSICAL_KEYCODE: u8 = 8;

/// Largest core X11 keycode.
pub const MAX_PHYSICAL_KEYCODE: u8 = u8::MAX;

/// Maximum click count in one action.
pub const MAX_CLICK_COUNT: u8 = 5;

/// Maximum discrete scroll notches in one action.
pub const MAX_SCROLL_COUNT: u16 = 1_000;

/// Maximum interval between discrete scroll notches.
pub const MAX_SCROLL_INTERVAL_MS: u16 = 1_000;

/// Maximum delay accepted by one XTEST fake-input request.
pub const MAX_XTEST_DELAY_MS: u16 = 10_000;

/// Initial fixed desktop double-click threshold.
pub const DEFAULT_DOUBLE_CLICK_THRESHOLD_MS: u16 = 250;

/// Maximum planned XTEST events in one complete compound action.
pub const MAX_INPUT_ACTION_EVENTS: usize = MAX_MOTION_EVENTS;

/// A non-zero X11 physical button detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalButton(u8);

impl PhysicalButton {
    /// Creates a physical button from the X11 request detail.
    pub fn new(detail: u8) -> Result<Self, InputActionError> {
        if detail == 0 {
            Err(InputActionError::InvalidPhysicalButton)
        } else {
            Ok(Self(detail))
        }
    }

    /// Returns the X11 request detail.
    #[must_use]
    pub const fn detail(self) -> u8 {
        self.0
    }
}

/// A core X11 physical keycode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalKey(u8);

impl PhysicalKey {
    /// Creates a structurally valid core X11 keycode.
    ///
    /// The active server's narrower minimum/maximum range is checked by the X11
    /// adapter after keymap resolution.
    pub fn new(keycode: u8) -> Result<Self, InputActionError> {
        if keycode < MIN_PHYSICAL_KEYCODE {
            Err(InputActionError::InvalidPhysicalKey { keycode })
        } else {
            Ok(Self(keycode))
        }
    }

    /// Returns the X11 keycode.
    #[must_use]
    pub const fn keycode(self) -> u8 {
        self.0
    }
}

/// A stable logical mouse-button name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalButton {
    /// Primary button, conventionally logical button 1.
    Left,
    /// Middle button, conventionally logical button 2.
    Middle,
    /// Secondary button, conventionally logical button 3.
    Right,
    /// Vertical scroll toward the top, logical button 4.
    ScrollUp,
    /// Vertical scroll toward the bottom, logical button 5.
    ScrollDown,
    /// Horizontal scroll toward the left, logical button 6.
    ScrollLeft,
    /// Horizontal scroll toward the right, logical button 7.
    ScrollRight,
    /// Browser-style back action, logical button 8.
    Back,
    /// Browser-style forward action, logical button 9.
    Forward,
}

impl LogicalButton {
    /// Returns the conventional logical button number.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::ScrollUp => 4,
            Self::ScrollDown => 5,
            Self::ScrollLeft => 6,
            Self::ScrollRight => 7,
            Self::Back => 8,
            Self::Forward => 9,
        }
    }
}

/// A checked inversion of X11 `GetPointerMapping` data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonMapping {
    logical_by_physical: Vec<u8>,
}

impl ButtonMapping {
    /// Stores a server pointer map, where index zero describes physical button 1.
    pub fn from_server(logical_by_physical: &[u8]) -> Result<Self, InputActionError> {
        if logical_by_physical.len() > usize::from(u8::MAX) {
            return Err(InputActionError::InvalidButtonMappingLength {
                actual: logical_by_physical.len(),
            });
        }
        Ok(Self {
            logical_by_physical: logical_by_physical.to_vec(),
        })
    }

    /// Resolves one logical name to a unique physical button.
    pub fn physical_for(&self, logical: LogicalButton) -> Result<PhysicalButton, InputActionError> {
        let logical_number = logical.number();
        let mut match_index = None;
        for (index, mapped_logical) in self.logical_by_physical.iter().enumerate() {
            if *mapped_logical == logical_number {
                if match_index.is_some() {
                    return Err(InputActionError::AmbiguousLogicalButton {
                        logical: logical_number,
                    });
                }
                match_index = Some(index);
            }
        }
        let index = match_index.ok_or(InputActionError::UnavailableLogicalButton {
            logical: logical_number,
        })?;
        let detail = u8::try_from(index.saturating_add(1))
            .map_err(|_| InputActionError::InvalidPhysicalButton)?;
        PhysicalButton::new(detail)
    }

    /// Returns the raw server map for diagnostics.
    #[must_use]
    pub fn as_server_map(&self) -> &[u8] {
        &self.logical_by_physical
    }
}

/// A raw button transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonDirection {
    /// Press the button.
    Down,
    /// Release the button.
    Up,
}

/// A validated pointer move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveAction {
    plan: MotionPlan,
}

impl MoveAction {
    /// Creates a move from a validated motion plan.
    #[must_use]
    pub const fn new(plan: MotionPlan) -> Self {
        Self { plan }
    }

    /// Returns the motion plan.
    #[must_use]
    pub const fn plan(&self) -> &MotionPlan {
        &self.plan
    }

    /// Returns the complete planned XTEST event count.
    #[must_use]
    pub fn xtest_event_count(&self) -> usize {
        self.plan.event_count()
    }
}

/// An atomic click action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickAction {
    movement: Option<MotionPlan>,
    button: LogicalButton,
    count: u8,
    pre_click_dwell_ms: u16,
    press_duration_ms: u16,
    inter_click_interval_ms: u16,
    xtest_event_count: usize,
}

impl ClickAction {
    /// Creates a bounded atomic click sequence.
    pub fn new(
        movement: Option<MotionPlan>,
        button: LogicalButton,
        count: u8,
        pre_click_dwell_ms: u16,
        press_duration_ms: u16,
        inter_click_interval_ms: u16,
        double_click_threshold_ms: u16,
    ) -> Result<Self, InputActionError> {
        if !(1..=MAX_CLICK_COUNT).contains(&count) {
            return Err(InputActionError::InvalidClickCount { count });
        }
        validate_xtest_delay(InputDelayKind::PreClickDwell, pre_click_dwell_ms)?;
        validate_xtest_delay(InputDelayKind::PressDuration, press_duration_ms)?;
        if double_click_threshold_ms == 0 || double_click_threshold_ms > MAX_XTEST_DELAY_MS {
            return Err(InputActionError::InvalidDoubleClickThreshold {
                threshold_ms: double_click_threshold_ms,
            });
        }
        if inter_click_interval_ms >= double_click_threshold_ms {
            return Err(InputActionError::InterClickIntervalTooLong {
                interval_ms: inter_click_interval_ms,
                threshold_ms: double_click_threshold_ms,
            });
        }
        let button_events = usize::from(count).saturating_mul(2);
        let xtest_event_count = movement
            .as_ref()
            .map_or(0, MotionPlan::event_count)
            .checked_add(button_events)
            .ok_or(InputActionError::EventLimitExceeded)?;
        validate_event_count(xtest_event_count)?;
        Ok(Self {
            movement,
            button,
            count,
            pre_click_dwell_ms,
            press_duration_ms,
            inter_click_interval_ms,
            xtest_event_count,
        })
    }

    /// Returns the optional movement performed once before clicking.
    #[must_use]
    pub const fn movement(&self) -> Option<&MotionPlan> {
        self.movement.as_ref()
    }

    /// Returns the logical button to resolve against the current server mapping.
    #[must_use]
    pub const fn logical_button(&self) -> LogicalButton {
        self.button
    }

    /// Returns the requested complete click count.
    #[must_use]
    pub const fn count(&self) -> u8 {
        self.count
    }

    /// Returns the press-request dwell.
    #[must_use]
    pub const fn pre_click_dwell_ms(&self) -> u16 {
        self.pre_click_dwell_ms
    }

    /// Returns the release-request delay.
    #[must_use]
    pub const fn press_duration_ms(&self) -> u16 {
        self.press_duration_ms
    }

    /// Returns the delay between complete clicks.
    #[must_use]
    pub const fn inter_click_interval_ms(&self) -> u16 {
        self.inter_click_interval_ms
    }

    /// Returns movement plus all press/release events.
    #[must_use]
    pub const fn xtest_event_count(&self) -> usize {
        self.xtest_event_count
    }
}

/// An atomic press, motion, and release drag action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragAction {
    movement: MotionPlan,
    button: LogicalButton,
    press_dwell_ms: u16,
    release_dwell_ms: u16,
    xtest_event_count: usize,
}

impl DragAction {
    /// Creates an atomic drag.
    pub fn new(
        movement: MotionPlan,
        button: LogicalButton,
        press_dwell_ms: u16,
        release_dwell_ms: u16,
    ) -> Result<Self, InputActionError> {
        validate_xtest_delay(InputDelayKind::DragPressDwell, press_dwell_ms)?;
        validate_xtest_delay(InputDelayKind::DragReleaseDwell, release_dwell_ms)?;
        let xtest_event_count = movement
            .event_count()
            // Press, an explicit no-op motion at `movement.start()` carrying
            // the pre-motion dwell, and release surround the planned samples.
            .checked_add(3)
            .ok_or(InputActionError::EventLimitExceeded)?;
        validate_event_count(xtest_event_count)?;
        Ok(Self {
            movement,
            button,
            press_dwell_ms,
            release_dwell_ms,
            xtest_event_count,
        })
    }

    /// Returns the motion performed while the button is owned.
    #[must_use]
    pub const fn movement(&self) -> &MotionPlan {
        &self.movement
    }

    /// Returns the logical button to resolve against the current server mapping.
    #[must_use]
    pub const fn logical_button(&self) -> LogicalButton {
        self.button
    }

    /// Returns the delay before movement begins.
    #[must_use]
    pub const fn press_dwell_ms(&self) -> u16 {
        self.press_dwell_ms
    }

    /// Returns the delay attached to release.
    #[must_use]
    pub const fn release_dwell_ms(&self) -> u16 {
        self.release_dwell_ms
    }

    /// Returns movement plus press, dwell-motion, and release events.
    #[must_use]
    pub const fn xtest_event_count(&self) -> usize {
        self.xtest_event_count
    }
}

/// The bounded XTEST delay being validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelayKind {
    /// Delay attached to click press.
    PreClickDwell,
    /// Delay attached to click release.
    PressDuration,
    /// Delay before drag motion.
    DragPressDwell,
    /// Delay attached to drag release.
    DragReleaseDwell,
}

fn validate_xtest_delay(kind: InputDelayKind, delay_ms: u16) -> Result<(), InputActionError> {
    if delay_ms > MAX_XTEST_DELAY_MS {
        Err(InputActionError::XtestDelayTooLong { kind, delay_ms })
    } else {
        Ok(())
    }
}

fn validate_event_count(event_count: usize) -> Result<(), InputActionError> {
    if event_count > MAX_INPUT_ACTION_EVENTS {
        Err(InputActionError::EventLimitExceeded)
    } else {
        Ok(())
    }
}

/// A discrete scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Scroll vertically upward.
    Up,
    /// Scroll vertically downward.
    Down,
    /// Scroll horizontally leftward.
    Left,
    /// Scroll horizontally rightward.
    Right,
}

impl ScrollDirection {
    /// Returns the logical core button used for discrete scrolling.
    #[must_use]
    pub const fn logical_button(self) -> LogicalButton {
        match self {
            Self::Up => LogicalButton::ScrollUp,
            Self::Down => LogicalButton::ScrollDown,
            Self::Left => LogicalButton::ScrollLeft,
            Self::Right => LogicalButton::ScrollRight,
        }
    }
}

/// A bounded sequence of discrete scroll notches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAction {
    direction: ScrollDirection,
    count: u16,
    interval_ms: u16,
}

impl ScrollAction {
    /// Creates a discrete scroll action whose button is resolved at execution.
    pub fn new(
        direction: ScrollDirection,
        count: u16,
        interval_ms: u16,
    ) -> Result<Self, InputActionError> {
        if !(1..=MAX_SCROLL_COUNT).contains(&count) {
            return Err(InputActionError::InvalidScrollCount { count });
        }
        if interval_ms > MAX_SCROLL_INTERVAL_MS {
            return Err(InputActionError::InvalidScrollInterval { interval_ms });
        }
        Ok(Self {
            direction,
            count,
            interval_ms,
        })
    }

    /// Returns the logical scroll button implied by the requested direction.
    #[must_use]
    pub const fn logical_button(self) -> LogicalButton {
        self.direction.logical_button()
    }

    /// Returns the requested direction.
    #[must_use]
    pub const fn direction(self) -> ScrollDirection {
        self.direction
    }

    /// Returns the notch count.
    #[must_use]
    pub const fn count(self) -> u16 {
        self.count
    }

    /// Returns the delay between complete notches.
    #[must_use]
    pub const fn interval_ms(self) -> u16 {
        self.interval_ms
    }

    /// Returns the complete press/release event count.
    #[must_use]
    pub const fn xtest_event_count(self) -> usize {
        self.count as usize * 2
    }
}

/// A validated physical key operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAction {
    plan: KeyPlan,
}

impl KeyAction {
    /// Creates a key action from a validated plan.
    #[must_use]
    pub const fn new(plan: KeyPlan) -> Self {
        Self { plan }
    }

    /// Returns the key transition plan.
    #[must_use]
    pub const fn plan(&self) -> &KeyPlan {
        &self.plan
    }

    /// Returns the balanced chord event count.
    #[must_use]
    pub fn xtest_event_count(&self) -> usize {
        self.plan.events().len()
    }
}

/// An admitted physical input operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// Move the pointer.
    Move(MoveAction),
    /// Execute one atomic click sequence.
    Click(ClickAction),
    /// Execute one atomic drag.
    Drag(DragAction),
    /// Emit discrete scroll buttons.
    Scroll(ScrollAction),
    /// Execute one key/chord plan.
    Key(KeyAction),
    /// Press or release a raw physical button.
    Button {
        /// Resolved physical button.
        button: PhysicalButton,
        /// Requested transition.
        direction: ButtonDirection,
        /// Whether duplicate down/up is accepted diagnostically.
        allow_redundant: bool,
    },
}

impl InputAction {
    /// Returns the complete planned XTEST event count, excluding recovery cleanup.
    #[must_use]
    pub fn xtest_event_count(&self) -> usize {
        match self {
            Self::Move(action) => action.xtest_event_count(),
            Self::Click(action) => action.xtest_event_count(),
            Self::Drag(action) => action.xtest_event_count(),
            Self::Scroll(action) => action.xtest_event_count(),
            Self::Key(action) => action.xtest_event_count(),
            Self::Button { .. } => 1,
        }
    }
}

/// Failure to validate a physical input action or mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputActionError {
    /// X11 detail zero is not a physical button.
    #[error("physical button detail must be non-zero")]
    InvalidPhysicalButton,
    /// A core X11 keycode is below the protocol minimum.
    #[error("physical keycode {keycode} is below {MIN_PHYSICAL_KEYCODE}")]
    InvalidPhysicalKey {
        /// Rejected keycode.
        keycode: u8,
    },
    /// The pointer mapping has an impossible physical-button count.
    #[error("pointer mapping length {actual} exceeds 255")]
    InvalidButtonMappingLength {
        /// Rejected map length.
        actual: usize,
    },
    /// More than one physical button maps to the requested logical button.
    #[error("logical button {logical} has an ambiguous physical mapping")]
    AmbiguousLogicalButton {
        /// Requested logical number.
        logical: u8,
    },
    /// No enabled physical button maps to the requested logical button.
    #[error("logical button {logical} is unavailable")]
    UnavailableLogicalButton {
        /// Requested logical number.
        logical: u8,
    },
    /// Click count is outside the action limit.
    #[error("click count {count} is outside 1..=5")]
    InvalidClickCount {
        /// Rejected count.
        count: u8,
    },
    /// A compound action would exceed the global planned event bound.
    #[error("compound input action exceeds {MAX_INPUT_ACTION_EVENTS} planned XTEST events")]
    EventLimitExceeded,
    /// A click or drag delay exceeds the backend primitive ceiling.
    #[error("{kind:?} delay {delay_ms} ms exceeds {MAX_XTEST_DELAY_MS} ms")]
    XtestDelayTooLong {
        /// Kind of delay.
        kind: InputDelayKind,
        /// Rejected delay.
        delay_ms: u16,
    },
    /// The configured double-click threshold is zero or exceeds XTEST bounds.
    #[error("double-click threshold {threshold_ms} ms is outside 1..=10000 ms")]
    InvalidDoubleClickThreshold {
        /// Rejected threshold.
        threshold_ms: u16,
    },
    /// The requested interval cannot be recognized as part of one multi-click.
    #[error("inter-click interval {interval_ms} ms must be below threshold {threshold_ms} ms")]
    InterClickIntervalTooLong {
        /// Rejected interval.
        interval_ms: u16,
        /// Fixed desktop threshold.
        threshold_ms: u16,
    },
    /// Scroll count is outside the action limit.
    #[error("scroll count {count} is outside 1..=1000")]
    InvalidScrollCount {
        /// Rejected count.
        count: u16,
    },
    /// Scroll interval is outside the action limit.
    #[error("scroll interval {interval_ms} ms exceeds 1000 ms")]
    InvalidScrollInterval {
        /// Rejected interval.
        interval_ms: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_mapping_is_inverted_and_ambiguity_is_rejected() -> Result<(), InputActionError> {
        let mapping = ButtonMapping::from_server(&[3, 2, 1, 4, 5])?;
        assert_eq!(
            mapping.physical_for(LogicalButton::Left)?,
            PhysicalButton::new(3)?
        );
        let ambiguous = ButtonMapping::from_server(&[1, 1, 2])?;
        assert_eq!(
            ambiguous.physical_for(LogicalButton::Left),
            Err(InputActionError::AmbiguousLogicalButton { logical: 1 })
        );
        Ok(())
    }

    #[test]
    fn click_delays_and_double_click_threshold_are_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let button = LogicalButton::Left;
        assert!(
            ClickAction::new(
                None,
                button,
                2,
                30,
                50,
                100,
                DEFAULT_DOUBLE_CLICK_THRESHOLD_MS
            )
            .is_ok()
        );
        assert_eq!(
            ClickAction::new(None, button, 2, 30, 50, 250, 250),
            Err(InputActionError::InterClickIntervalTooLong {
                interval_ms: 250,
                threshold_ms: 250
            })
        );
        assert_eq!(
            ClickAction::new(None, button, 1, 10_001, 50, 0, 250),
            Err(InputActionError::XtestDelayTooLong {
                kind: InputDelayKind::PreClickDwell,
                delay_ms: 10_001
            })
        );
        Ok(())
    }

    #[test]
    fn compound_actions_retain_logical_button_intent() -> Result<(), Box<dyn std::error::Error>> {
        let movement = crate::input::plan_motion(
            crate::domain::RootPoint::new(0, 0)?,
            crate::domain::RootPoint::new(1, 1)?,
            crate::input::MotionOptions::instant(false),
        )?;
        let click = ClickAction::new(
            None,
            LogicalButton::Right,
            1,
            0,
            0,
            0,
            DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
        )?;
        let drag = DragAction::new(movement, LogicalButton::Middle, 0, 0)?;
        assert_eq!(click.logical_button(), LogicalButton::Right);
        assert_eq!(drag.logical_button(), LogicalButton::Middle);
        assert_eq!(drag.xtest_event_count(), drag.movement().event_count() + 3);
        assert_eq!(
            drag.movement().start(),
            crate::domain::RootPoint::new(0, 0)?
        );

        for (direction, logical) in [
            (ScrollDirection::Up, LogicalButton::ScrollUp),
            (ScrollDirection::Down, LogicalButton::ScrollDown),
            (ScrollDirection::Left, LogicalButton::ScrollLeft),
            (ScrollDirection::Right, LogicalButton::ScrollRight),
        ] {
            let scroll = ScrollAction::new(direction, 1, 0)?;
            assert_eq!(scroll.logical_button(), logical);
        }
        Ok(())
    }

    #[test]
    fn raw_button_action_remains_explicitly_physical() -> Result<(), InputActionError> {
        let physical = PhysicalButton::new(7)?;
        let action = InputAction::Button {
            button: physical,
            direction: ButtonDirection::Down,
            allow_redundant: false,
        };
        assert!(matches!(
            action,
            InputAction::Button { button, .. } if button == physical
        ));
        Ok(())
    }
}
