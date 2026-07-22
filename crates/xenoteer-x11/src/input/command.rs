//! Queue-level input command types.

use std::time::Instant;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xenoteer_core::{
    domain::{PointerDelta, RootPoint},
    input::{InputAction, LogicalButton, MotionOptions, ScrollAction},
};
use xenoteer_protocol::CommandId;
use xenoteer_protocol::{CoordinateSpace, Point};

use super::{InputFailure, InputOutcome, KeyboardAction};

/// An absolute move planned from the actor's immediate execution-time pointer.
///
/// Keeping the intent unresolved until it reaches the single-owner X11 actor
/// prevents queued smooth moves from using a stale start after an earlier
/// queued command moved the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerMoveRequest {
    target: RootPoint,
    options: MotionOptions,
}

/// An endpoint resolved against the actor's immediate execution-time pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEndpoint {
    /// Absolute root-physical endpoint.
    Root(RootPoint),
    /// Signed displacement from the execution-time pointer position.
    Relative(PointerDelta),
}

/// A relative move planned from the actor's immediate execution-time pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerMoveRelativeRequest {
    delta: PointerDelta,
    options: MotionOptions,
}

impl PointerMoveRelativeRequest {
    /// Creates an execution-time relative motion request.
    #[must_use]
    pub const fn new(delta: PointerDelta, options: MotionOptions) -> Self {
        Self { delta, options }
    }

    /// Returns the requested pointer displacement.
    #[must_use]
    pub const fn delta(self) -> PointerDelta {
        self.delta
    }

    /// Returns validated interpolation options.
    #[must_use]
    pub const fn options(self) -> MotionOptions {
        self.options
    }
}

/// An unresolved complete click built only after the live pointer is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerClickRequest {
    endpoint: Option<PointerEndpoint>,
    options: MotionOptions,
    button: LogicalButton,
    count: u8,
    pre_click_dwell_ms: u16,
    press_duration_ms: u16,
    inter_click_interval_ms: u16,
}

impl PointerClickRequest {
    /// Creates a click request whose optional movement is actor-time resolved.
    #[must_use]
    pub const fn new(
        endpoint: Option<PointerEndpoint>,
        options: MotionOptions,
        button: LogicalButton,
        count: u8,
        pre_click_dwell_ms: u16,
        press_duration_ms: u16,
        inter_click_interval_ms: u16,
    ) -> Self {
        Self {
            endpoint,
            options,
            button,
            count,
            pre_click_dwell_ms,
            press_duration_ms,
            inter_click_interval_ms,
        }
    }

    pub(super) const fn endpoint(self) -> Option<PointerEndpoint> {
        self.endpoint
    }
    pub(super) const fn options(self) -> MotionOptions {
        self.options
    }
    pub(super) const fn button(self) -> LogicalButton {
        self.button
    }
    pub(super) const fn count(self) -> u8 {
        self.count
    }
    pub(super) const fn pre_click_dwell_ms(self) -> u16 {
        self.pre_click_dwell_ms
    }
    pub(super) const fn press_duration_ms(self) -> u16 {
        self.press_duration_ms
    }
    pub(super) const fn inter_click_interval_ms(self) -> u16 {
        self.inter_click_interval_ms
    }
}

/// An unresolved complete drag built only after the live pointer is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerDragRequest {
    endpoint: PointerEndpoint,
    options: MotionOptions,
    button: LogicalButton,
    press_dwell_ms: u16,
    release_dwell_ms: u16,
}

/// A window-local click resolved from live actor-owned X11 geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPointerClickRequest {
    window: u32,
    coordinate_space: CoordinateSpace,
    point: Point,
    bounds_policy: WindowPointerBoundsPolicy,
    options: MotionOptions,
    button: LogicalButton,
    count: u8,
    pre_click_dwell_ms: u16,
    press_duration_ms: u16,
    inter_click_interval_ms: u16,
}

/// Policy for a local point outside the selected live client/frame rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPointerBoundsPolicy {
    /// Reject the click before effect.
    Reject,
    /// Clamp to the nearest in-window point.
    Clamp,
    /// Permit an outside-window point if translation still fits XTEST.
    Allow,
}

impl WindowPointerClickRequest {
    /// Creates an unresolved exact-window click request.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        window: u32,
        coordinate_space: CoordinateSpace,
        point: Point,
        bounds_policy: WindowPointerBoundsPolicy,
        options: MotionOptions,
        button: LogicalButton,
        count: u8,
        pre_click_dwell_ms: u16,
        press_duration_ms: u16,
        inter_click_interval_ms: u16,
    ) -> Self {
        Self {
            window,
            coordinate_space,
            point,
            bounds_policy,
            options,
            button,
            count,
            pre_click_dwell_ms,
            press_duration_ms,
            inter_click_interval_ms,
        }
    }

    pub(super) const fn window(self) -> u32 {
        self.window
    }
    pub(super) const fn coordinate_space(self) -> CoordinateSpace {
        self.coordinate_space
    }
    pub(super) const fn point(self) -> Point {
        self.point
    }
    pub(super) const fn bounds_policy(self) -> WindowPointerBoundsPolicy {
        self.bounds_policy
    }
    pub(super) const fn options(self) -> MotionOptions {
        self.options
    }
    pub(super) const fn button(self) -> LogicalButton {
        self.button
    }
    pub(super) const fn count(self) -> u8 {
        self.count
    }
    pub(super) const fn pre_click_dwell_ms(self) -> u16 {
        self.pre_click_dwell_ms
    }
    pub(super) const fn press_duration_ms(self) -> u16 {
        self.press_duration_ms
    }
    pub(super) const fn inter_click_interval_ms(self) -> u16 {
        self.inter_click_interval_ms
    }
}

impl PointerDragRequest {
    /// Creates an actor-time resolved press/move/release request.
    #[must_use]
    pub const fn new(
        endpoint: PointerEndpoint,
        options: MotionOptions,
        button: LogicalButton,
        press_dwell_ms: u16,
        release_dwell_ms: u16,
    ) -> Self {
        Self {
            endpoint,
            options,
            button,
            press_dwell_ms,
            release_dwell_ms,
        }
    }

    pub(super) const fn endpoint(self) -> PointerEndpoint {
        self.endpoint
    }
    pub(super) const fn options(self) -> MotionOptions {
        self.options
    }
    pub(super) const fn button(self) -> LogicalButton {
        self.button
    }
    pub(super) const fn press_dwell_ms(self) -> u16 {
        self.press_dwell_ms
    }
    pub(super) const fn release_dwell_ms(self) -> u16 {
        self.release_dwell_ms
    }
}

/// Failure reported by a daemon-supplied owner-thread near-effect check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPreconditionFailure {
    /// The exact observed target birth is no longer current.
    TargetStale,
    /// The exact target no longer owns keyboard focus.
    FocusLost,
    /// Required observation evidence could not be obtained.
    Unavailable,
}

/// Content-free near-effect validator executed by the input owner thread.
///
/// Window-targeted multi-click operations evaluate the same exact-target
/// validator before every button press. This prevents a later click in the
/// atomic sequence from landing on a replacement or newly focused window.
pub struct InputPrecondition {
    check: Box<dyn FnMut() -> Result<(), InputPreconditionFailure> + Send + 'static>,
}

impl InputPrecondition {
    /// Wraps a bounded synchronous validation closure.
    pub fn new<F>(check: F) -> Self
    where
        F: FnMut() -> Result<(), InputPreconditionFailure> + Send + 'static,
    {
        Self {
            check: Box::new(check),
        }
    }

    pub(super) fn evaluate(&mut self) -> Result<(), InputPreconditionFailure> {
        (self.check)()
    }
}

impl core::fmt::Debug for InputPrecondition {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("InputPrecondition(<redacted>)")
    }
}

impl PointerMoveRequest {
    /// Creates an execution-time absolute motion request.
    #[must_use]
    pub const fn new(target: RootPoint, options: MotionOptions) -> Self {
        Self { target, options }
    }

    /// Returns the requested absolute root endpoint.
    #[must_use]
    pub const fn target(self) -> RootPoint {
        self.target
    }

    /// Returns the validated interpolation options.
    #[must_use]
    pub const fn options(self) -> MotionOptions {
        self.options
    }
}

/// One FIFO-serialized pointer/button or unresolved keyboard operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOperation {
    /// Already validated backend-independent pointer/button action.
    Pointer(InputAction),
    /// Absolute motion whose start is observed only when execution begins.
    PointerMove(PointerMoveRequest),
    /// Relative motion whose start and endpoint are observed when execution begins.
    PointerMoveRelative(PointerMoveRelativeRequest),
    /// Complete click whose movement is resolved at execution time.
    PointerClick(PointerClickRequest),
    /// Complete drag whose movement is resolved at execution time.
    PointerDrag(PointerDragRequest),
    /// Exact-window click whose root endpoint is resolved from live geometry.
    WindowPointerClick(WindowPointerClickRequest),
    /// Already validated complete discrete scroll action.
    PointerScroll(ScrollAction),
    /// Bounded keyboard intent resolved only by the actor-owned live model.
    Keyboard(KeyboardAction),
}

/// Identity and monotonic execution deadline carried through the actor queue.
#[derive(Debug, Clone, Copy)]
pub struct ActionContext {
    /// Public command identifier used for result correlation.
    pub command_id: CommandId,
    /// Optional monotonic deadline; wall-clock changes cannot extend it.
    pub deadline: Option<Instant>,
}

impl ActionContext {
    /// Builds action context for one admitted command.
    #[must_use]
    pub const fn new(command_id: CommandId, deadline: Option<Instant>) -> Self {
        Self {
            command_id,
            deadline,
        }
    }
}

/// One validated ordinary command sent through the bounded FIFO queue.
#[derive(Debug)]
pub struct InputCommand {
    /// Queue-level identity and deadline.
    pub context: ActionContext,
    /// Pointer action or unresolved keyboard intent.
    pub operation: InputOperation,
    /// Optional exact-target/focus check executed on the owner thread near effect.
    pub precondition: Option<InputPrecondition>,
    /// Cooperative cancellation signal checked only at documented safe boundaries.
    pub cancellation: CancellationToken,
    /// Exactly one terminal actor result.
    pub reply: oneshot::Sender<Result<InputOutcome, InputFailure>>,
}
