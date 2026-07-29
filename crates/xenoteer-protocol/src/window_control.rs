//! Desired-state EWMH command payloads and observed window-control evidence.

#![allow(missing_docs)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::window::{
    MAX_WINDOW_DIMENSION, MAX_WINDOW_WARNINGS, StrictWindowRef, WindowGeometry,
    WindowModelRevision, WindowRect, WindowRef, WindowSnapshot, WindowValidationError,
    deserialize_optional_strict_window_ref, deserialize_strict_window_ref,
};
use crate::{CoordinateSpace, DesktopGeneration, DesktopId};

/// Maximum operation-level entries in one window-manager capability report.
pub const MAX_WINDOW_MANAGER_CAPABILITIES: usize = 16;
/// Largest zero-based EWMH workspace index accepted as an ordinary desktop.
///
/// `u32::MAX` is reserved by EWMH for the all-desktops/sticky sentinel and is
/// deliberately unavailable through the move-to-workspace command.
pub const MAX_WINDOW_WORKSPACE_INDEX: u32 = u32::MAX - 1;

/// Explicit fallback policy for activation when EWMH cannot establish focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowFocusFallback {
    /// Use only `_NET_ACTIVE_WINDOW` and observed WM/core-focus convergence.
    EwmhOnly,
    /// Permit the documented raw `SetInputFocus` compatibility fallback.
    AllowSetInputFocus,
}

/// Requests activation through the cooperating window manager.
///
/// Admission and near-effect revalidation require `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowActivateCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    /// Whether a fixed-profile workspace switch may precede activation.
    pub switch_workspace: bool,
    pub fallback: WindowFocusFallback,
}

impl WindowActivateCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape().map_err(Into::into)
    }
}

/// Postcondition the close saga should await after sending its advisory request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCloseWaitPolicy {
    /// Return after a same-connection barrier proves the request was sent.
    RequestSent,
    /// Await destruction or unmapping until the command deadline.
    UnmappedOrDestroyed,
}

/// Requests `_NET_CLOSE_WINDOW`, or the proven `WM_DELETE_WINDOW` fallback.
///
/// This command never implies process termination and requires `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowCloseCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    pub wait_for: WindowCloseWaitPolicy,
}

impl WindowCloseCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape().map_err(Into::into)
    }
}

/// Desired idempotent `_NET_WM_STATE` projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowManagerState {
    /// Both vertical and horizontal maximize atoms as one public state.
    Maximized,
    Fullscreen,
    Above,
    Sticky,
}

/// One operation-level capability projected from root `_NET_SUPPORTED` atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowManagerCapability {
    Activate,
    Close,
    StateMaximized,
    StateFullscreen,
    StateAbove,
    StateSticky,
    MoveResize,
    MoveToWorkspace,
    ClientList,
    StackingList,
    FrameExtents,
    CurrentWorkspace,
    WindowWorkspace,
}

/// Generation-scoped window-manager support observed at one model revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowManagerCapabilities {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub model_revision: WindowModelRevision,
    #[schemars(length(max = MAX_WINDOW_MANAGER_CAPABILITIES))]
    pub supported: Vec<WindowManagerCapability>,
}

impl WindowManagerCapabilities {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        if self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
            || self.supported.len() > MAX_WINDOW_MANAGER_CAPABILITIES
            || has_duplicates(&self.supported)
        {
            return Err(WindowControlValidationError::Capabilities);
        }
        Ok(())
    }

    /// Returns whether the current generation advertised one operation.
    #[must_use]
    pub fn supports(&self, capability: WindowManagerCapability) -> bool {
        self.supported.contains(&capability)
    }
}

/// Adds or removes one supported window-manager state; public toggle is omitted.
///
/// Admission and near-effect revalidation require `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowSetStateCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    pub state: WindowManagerState,
    /// Requested final state, preserving retry idempotency.
    pub desired: bool,
}

impl WindowSetStateCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape().map_err(Into::into)
    }
}

/// Requests the desired minimized state without setting WM-owned hidden atoms.
///
/// Admission and near-effect revalidation require `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowMinimizeCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    pub desired: bool,
}

impl WindowMinimizeCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape().map_err(Into::into)
    }
}

/// Whether requested geometry describes the WM frame or top-level client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowGeometryTarget {
    Frame,
    Client,
}

/// Policy for a desired rectangle that would extend beyond root bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowScreenBoundsPolicy {
    /// Reject geometry outside the root rectangle before the WM request.
    RequireInsideRoot,
    /// Clamp the desired geometry to the root and report a constraint warning.
    ClampToRoot,
    /// Permit a cooperating WM to place part of the window off-screen.
    AllowOffscreen,
}

/// Partial desired geometry; omitted axes/extents retain the current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowGeometryRequest {
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[schemars(range(min = 1, max = MAX_WINDOW_DIMENSION))]
    pub width: Option<u32>,
    #[schemars(range(min = 1, max = MAX_WINDOW_DIMENSION))]
    pub height: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "WindowGeometryRequest")]
struct StrictWindowGeometryRequest {
    x: Option<i32>,
    y: Option<i32>,
    #[schemars(range(min = 1, max = MAX_WINDOW_DIMENSION))]
    width: Option<u32>,
    #[schemars(range(min = 1, max = MAX_WINDOW_DIMENSION))]
    height: Option<u32>,
}

impl From<StrictWindowGeometryRequest> for WindowGeometryRequest {
    fn from(value: StrictWindowGeometryRequest) -> Self {
        Self {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

fn deserialize_strict_window_geometry_request<'de, D>(
    deserializer: D,
) -> Result<WindowGeometryRequest, D::Error>
where
    D: serde::Deserializer<'de>,
{
    StrictWindowGeometryRequest::deserialize(deserializer).map(Into::into)
}

impl WindowGeometryRequest {
    /// Requires at least one requested field and valid non-zero dimensions.
    pub fn validate(self) -> Result<(), WindowControlValidationError> {
        if self.x.is_none() && self.y.is_none() && self.width.is_none() && self.height.is_none() {
            return Err(WindowControlValidationError::EmptyGeometry);
        }
        if self
            .width
            .is_some_and(|value| value == 0 || value > MAX_WINDOW_DIMENSION)
            || self
                .height
                .is_some_and(|value| value == 0 || value > MAX_WINDOW_DIMENSION)
        {
            return Err(WindowControlValidationError::GeometryDimension);
        }
        Ok(())
    }
}

/// Requests idempotent programmatic move/resize through `_NET_MOVERESIZE_WINDOW`.
///
/// Admission and near-effect revalidation require `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowMoveResizeCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    pub relative_to: WindowGeometryTarget,
    #[serde(deserialize_with = "deserialize_strict_window_geometry_request")]
    #[schemars(with = "StrictWindowGeometryRequest")]
    pub geometry: WindowGeometryRequest,
    pub bounds_policy: WindowScreenBoundsPolicy,
}

impl WindowMoveResizeCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape()?;
        self.geometry.validate()
    }
}

/// Requests moving one exact window birth to a zero-based EWMH workspace.
///
/// Admission and near-effect revalidation require `window:control`. The
/// execution layer must additionally prove that the requested index is below
/// the live `_NET_NUMBER_OF_DESKTOPS` value before sending `_NET_WM_DESKTOP`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowMoveToWorkspaceCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    /// Desired zero-based EWMH desktop index; the sticky sentinel is forbidden.
    #[schemars(range(max = MAX_WINDOW_WORKSPACE_INDEX))]
    pub workspace: u32,
}

impl WindowMoveToWorkspaceCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.window.validate_shape()?;
        if self.workspace == u32::MAX {
            return Err(WindowControlValidationError::Workspace);
        }
        Ok(())
    }
}

/// Advisory stacking operation; exact global z-order is never promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowStackMode {
    Raise,
    Lower,
    Above,
    Below,
}

/// Requests best-effort stacking relative to the WM or an exact sibling birth.
///
/// Admission and near-effect revalidation require `window:control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowStackCommand {
    #[serde(deserialize_with = "deserialize_strict_window_ref")]
    #[schemars(with = "StrictWindowRef")]
    pub window: WindowRef,
    pub mode: WindowStackMode,
    /// Required by `above`/`below` and forbidden by `raise`/`lower`.
    #[serde(deserialize_with = "deserialize_optional_strict_window_ref")]
    #[schemars(with = "Option<StrictWindowRef>")]
    pub sibling: Option<WindowRef>,
}

impl WindowStackCommand {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        validate_stack_request(&self.window, self.mode, self.sibling.as_ref())
    }
}

/// Safe bounded warning produced while observing an advisory WM operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowControlWarning {
    UsedSetInputFocusFallback,
    UsedWmDeleteWindowFallback,
    UsedRawStackFallback,
    CurrentTimeFallback,
    GeometryConstrained,
    WorkspaceNotConfirmed,
    PartialStateObserved,
    FocusNotAcquired,
    StackingNotConfirmed,
    TargetUnmapped,
}

/// Observed activation/focus evidence after an activation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowActivateResult {
    pub requested: WindowRef,
    pub observed_active: Option<WindowRef>,
    pub observed_focused: Option<WindowRef>,
    pub converged: bool,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowActivateResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        validate_result_references(
            &self.requested,
            [
                self.observed_active.as_ref(),
                self.observed_focused.as_ref(),
            ],
        )?;
        if self.converged
            != (self.observed_active.as_ref() == Some(&self.requested)
                && self.observed_focused.as_ref() == Some(&self.requested))
            || self.converged
                && self
                    .warnings
                    .contains(&WindowControlWarning::FocusNotAcquired)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::UsedSetInputFocusFallback,
                WindowControlWarning::CurrentTimeFallback,
                WindowControlWarning::FocusNotAcquired,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// Observed terminal state of a close request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCloseOutcome {
    Destroyed,
    Unmapped,
    RefusedOrTimedOut,
    ProcessExited,
    RequestSent,
}

/// Close evidence that never implies a force-kill operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowCloseResult {
    pub requested: WindowRef,
    pub outcome: WindowCloseOutcome,
    /// Last usable observation when the original still exists.
    pub final_snapshot: Option<WindowSnapshot>,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowCloseResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.requested.validate_shape()?;
        if let Some(snapshot) = &self.final_snapshot {
            snapshot.validate()?;
            if snapshot.window != self.requested {
                return Err(WindowControlValidationError::ObservedResult);
            }
        }
        if self.outcome == WindowCloseOutcome::Destroyed && self.final_snapshot.is_some() {
            return Err(WindowControlValidationError::ObservedResult);
        }
        if self.outcome == WindowCloseOutcome::Unmapped
            && self.final_snapshot.as_ref().is_none_or(|snapshot| {
                snapshot.state.map_state != crate::window::WindowMapState::Unmapped
            })
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::UsedWmDeleteWindowFallback,
                WindowControlWarning::CurrentTimeFallback,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// State operation named by an observed convergence result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WindowStateOperation {
    ManagerState { state: WindowManagerState },
    Minimized,
}

/// Normalized observation of a desired boolean state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowStateObservation {
    Enabled,
    Disabled,
    /// Only part of a compound state, currently maximization, was observed.
    Partial,
}

impl WindowStateObservation {
    const fn satisfies(self, desired: bool) -> bool {
        matches!(
            (self, desired),
            (Self::Enabled, true) | (Self::Disabled, false)
        )
    }
}

/// Observed desired-state convergence for state/minimize operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowStateResult {
    pub requested: WindowRef,
    pub operation: WindowStateOperation,
    pub desired: bool,
    pub observed: WindowStateObservation,
    pub converged: bool,
    pub final_snapshot: WindowSnapshot,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowStateResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.requested.validate_shape()?;
        self.final_snapshot.validate()?;
        if self.final_snapshot.window != self.requested
            || self.observed != observe_state(self.operation, &self.final_snapshot)
            || self.converged != self.observed.satisfies(self.desired)
            || (self.observed == WindowStateObservation::Partial)
                != self
                    .warnings
                    .contains(&WindowControlWarning::PartialStateObserved)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        if matches!(self.operation, WindowStateOperation::ManagerState { .. })
            && self
                .warnings
                .contains(&WindowControlWarning::CurrentTimeFallback)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::PartialStateObserved,
                WindowControlWarning::CurrentTimeFallback,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// Desired versus observed geometry after the WM quiet window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowMoveResizeResult {
    pub requested: WindowRef,
    pub relative_to: WindowGeometryTarget,
    pub desired: WindowGeometryRequest,
    /// Full root-physical frame/client rectangle after live bounds/frame normalization.
    pub effective: WindowRect,
    pub observed: WindowGeometry,
    pub observed_revision: WindowModelRevision,
    pub constrained: bool,
    pub converged: bool,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowMoveResizeResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.requested.validate_shape()?;
        self.desired.validate()?;
        self.effective.validate()?;
        self.observed.validate()?;
        if self.effective.coordinate_space != CoordinateSpace::RootPhysical {
            return Err(WindowControlValidationError::ObservedResult);
        }
        let observed_rect = match self.relative_to {
            WindowGeometryTarget::Frame => self.observed.frame_rect,
            WindowGeometryTarget::Client => Some(self.observed.client_rect),
        };
        let desired_matches =
            observed_rect.is_some_and(|rect| geometry_request_matches(rect, self.desired));
        if (!desired_matches && !self.constrained)
            || (self.converged && observed_rect != Some(self.effective))
            || self.constrained
                != self
                    .warnings
                    .contains(&WindowControlWarning::GeometryConstrained)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::GeometryConstrained,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// Observed convergence after requesting a zero-based workspace assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowMoveToWorkspaceResult {
    pub requested: WindowRef,
    #[schemars(range(max = MAX_WINDOW_WORKSPACE_INDEX))]
    pub desired_workspace: u32,
    /// Last normalized `_NET_WM_DESKTOP` value, if the property was usable.
    pub observed_workspace: Option<u32>,
    pub observed_revision: WindowModelRevision,
    pub converged: bool,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowMoveToWorkspaceResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        self.requested.validate_shape()?;
        if self.desired_workspace == u32::MAX
            || self.observed_workspace == Some(u32::MAX)
            || self.converged != (self.observed_workspace == Some(self.desired_workspace))
            || self.converged
                == self
                    .warnings
                    .contains(&WindowControlWarning::WorkspaceNotConfirmed)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::WorkspaceNotConfirmed,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// Best-effort stacking convergence evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowStackResult {
    pub requested: WindowRef,
    pub mode: WindowStackMode,
    pub sibling: Option<WindowRef>,
    pub observed_stacking_index: Option<u32>,
    pub observed_sibling_index: Option<u32>,
    pub observed_revision: WindowModelRevision,
    pub converged: bool,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowControlWarning>,
}

impl WindowStackResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        validate_stack_request(&self.requested, self.mode, self.sibling.as_ref())?;
        let derived_relative = match (
            self.mode,
            self.observed_stacking_index,
            self.observed_sibling_index,
        ) {
            (WindowStackMode::Above, Some(requested), Some(sibling)) => Some(requested > sibling),
            (WindowStackMode::Below, Some(requested), Some(sibling)) => Some(requested < sibling),
            (WindowStackMode::Above | WindowStackMode::Below, _, _) => Some(false),
            (WindowStackMode::Raise | WindowStackMode::Lower, _, None) => None,
            (WindowStackMode::Raise | WindowStackMode::Lower, _, Some(_)) => {
                return Err(WindowControlValidationError::ObservedResult);
            }
        };
        if derived_relative.is_some_and(|derived| derived != self.converged)
            || self.converged
                == self
                    .warnings
                    .contains(&WindowControlWarning::StackingNotConfirmed)
        {
            return Err(WindowControlValidationError::ObservedResult);
        }
        validate_warnings_for(
            &self.warnings,
            &[
                WindowControlWarning::UsedRawStackFallback,
                WindowControlWarning::StackingNotConfirmed,
                WindowControlWarning::TargetUnmapped,
            ],
        )
    }
}

/// Typed outcome ready to become one `CommandOutcome` variant at integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WindowControlResult {
    Activated(Box<WindowActivateResult>),
    Closed(Box<WindowCloseResult>),
    StateChanged(Box<WindowStateResult>),
    Minimized(Box<WindowStateResult>),
    GeometryChanged(Box<WindowMoveResizeResult>),
    WorkspaceChanged(Box<WindowMoveToWorkspaceResult>),
    Stacked(Box<WindowStackResult>),
}

impl WindowControlResult {
    pub fn validate(&self) -> Result<(), WindowControlValidationError> {
        match self {
            Self::Activated(value) => value.validate(),
            Self::Closed(value) => value.validate(),
            Self::StateChanged(value) => {
                if !matches!(value.operation, WindowStateOperation::ManagerState { .. }) {
                    return Err(WindowControlValidationError::ObservedResult);
                }
                value.validate()
            }
            Self::Minimized(value) => {
                if value.operation != WindowStateOperation::Minimized {
                    return Err(WindowControlValidationError::ObservedResult);
                }
                value.validate()
            }
            Self::GeometryChanged(value) => value.validate(),
            Self::WorkspaceChanged(value) => value.validate(),
            Self::Stacked(value) => value.validate(),
        }
    }
}

fn validate_stack_request(
    window: &WindowRef,
    mode: WindowStackMode,
    sibling: Option<&WindowRef>,
) -> Result<(), WindowControlValidationError> {
    window.validate_shape()?;
    match (mode, sibling) {
        (WindowStackMode::Above | WindowStackMode::Below, Some(sibling)) => {
            sibling.validate_shape()?;
            if !window.shares_desktop_scope(sibling) || window == sibling {
                return Err(WindowControlValidationError::StackSibling);
            }
        }
        (WindowStackMode::Raise | WindowStackMode::Lower, None) => {}
        _ => return Err(WindowControlValidationError::StackSibling),
    }
    Ok(())
}

fn observe_state(
    operation: WindowStateOperation,
    snapshot: &WindowSnapshot,
) -> WindowStateObservation {
    let has_atom = |atom: &str| {
        snapshot
            .metadata
            .states
            .iter()
            .any(|observed| observed.as_str() == atom)
    };
    match operation {
        WindowStateOperation::Minimized => {
            if snapshot.state.minimized {
                WindowStateObservation::Enabled
            } else {
                WindowStateObservation::Disabled
            }
        }
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Maximized,
        } => match (
            has_atom("_NET_WM_STATE_MAXIMIZED_VERT"),
            has_atom("_NET_WM_STATE_MAXIMIZED_HORZ"),
        ) {
            (true, true) => WindowStateObservation::Enabled,
            (false, false) => WindowStateObservation::Disabled,
            _ => WindowStateObservation::Partial,
        },
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Fullscreen,
        } => observation_from_bool(has_atom("_NET_WM_STATE_FULLSCREEN")),
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Above,
        } => observation_from_bool(has_atom("_NET_WM_STATE_ABOVE")),
        WindowStateOperation::ManagerState {
            state: WindowManagerState::Sticky,
        } => observation_from_bool(snapshot.state.sticky),
    }
}

const fn observation_from_bool(value: bool) -> WindowStateObservation {
    if value {
        WindowStateObservation::Enabled
    } else {
        WindowStateObservation::Disabled
    }
}

fn geometry_request_matches(rect: WindowRect, request: WindowGeometryRequest) -> bool {
    let origin = rect.rect.origin();
    let Ok(size) = rect.rect.size() else {
        return false;
    };
    request.x.is_none_or(|value| value == origin.x())
        && request.y.is_none_or(|value| value == origin.y())
        && request.width.is_none_or(|value| value == size.width())
        && request.height.is_none_or(|value| value == size.height())
}

fn validate_result_references<'a>(
    requested: &WindowRef,
    related: impl IntoIterator<Item = Option<&'a WindowRef>>,
) -> Result<(), WindowControlValidationError> {
    requested.validate_shape()?;
    for reference in related.into_iter().flatten() {
        reference.validate_shape()?;
        if !requested.shares_desktop_scope(reference) {
            return Err(WindowControlValidationError::ObservedResult);
        }
    }
    Ok(())
}

fn validate_warnings(
    warnings: &[WindowControlWarning],
) -> Result<(), WindowControlValidationError> {
    if warnings.len() > MAX_WINDOW_WARNINGS || has_duplicates(warnings) {
        return Err(WindowControlValidationError::ObservedResult);
    }
    Ok(())
}

fn validate_warnings_for(
    warnings: &[WindowControlWarning],
    allowed: &[WindowControlWarning],
) -> Result<(), WindowControlValidationError> {
    validate_warnings(warnings)?;
    if warnings.iter().any(|warning| !allowed.contains(warning)) {
        return Err(WindowControlValidationError::ObservedResult);
    }
    Ok(())
}

fn has_duplicates<T: Eq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

/// Window-control payload validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WindowControlValidationError {
    #[error(transparent)]
    Window(#[from] WindowValidationError),
    #[error("window geometry request must change at least one field")]
    EmptyGeometry,
    #[error("window geometry request has an invalid dimension")]
    GeometryDimension,
    #[error("stacking sibling does not match the requested mode or desktop scope")]
    StackSibling,
    #[error("workspace index uses the reserved all-desktops sentinel")]
    Workspace,
    #[error("observed window-control result is internally inconsistent")]
    ObservedResult,
    #[error("window-manager capability projection is invalid")]
    Capabilities,
}

#[cfg(test)]
mod tests {
    use crate::{DesktopGeneration, DesktopId, Rect};

    use super::*;
    use crate::window::{
        WINDOW_IDENTITY_HASH_BYTES, WindowAtomName, WindowIdentityHash, WindowMapState,
        WindowMetadata, WindowObservedState, WindowProcessConfidence, WindowProcessCorrelation,
    };

    fn reference(xid: u32) -> Result<WindowRef, WindowValidationError> {
        Ok(WindowRef {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            xid,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new("c".repeat(WINDOW_IDENTITY_HASH_BYTES))?,
        })
    }

    fn snapshot(
        window: WindowRef,
        states: Vec<WindowAtomName>,
    ) -> Result<WindowSnapshot, WindowValidationError> {
        Ok(WindowSnapshot {
            xid_hex: window.xid_hex(),
            window,
            model_revision: WindowModelRevision::new(4)?,
            metadata: WindowMetadata {
                title: None,
                visible_title: None,
                icon_title: None,
                class: None,
                client_machine: None,
                window_types: Vec::new(),
                states,
                allowed_actions: Vec::new(),
                protocols: Vec::new(),
            },
            process: WindowProcessCorrelation {
                reported_pid: None,
                managed_process: None,
                confidence: WindowProcessConfidence::None,
                evidence: Vec::new(),
                conflict: false,
            },
            state: WindowObservedState {
                map_state: WindowMapState::Viewable,
                minimized: false,
                hidden: false,
                urgent: false,
                modal: false,
                sticky: false,
                active: false,
                focused: false,
            },
            geometry: None,
            workspace: Some(0),
            client_leader: None,
            transient_for: None,
            group_leader: None,
            stacking_index: Some(0),
            has_accessibility_application: false,
            warnings: Vec::new(),
        })
    }

    #[test]
    fn desired_state_has_no_retry_unsafe_toggle_variant() -> Result<(), Box<dyn std::error::Error>>
    {
        let command = WindowSetStateCommand {
            window: reference(11)?,
            state: WindowManagerState::Fullscreen,
            desired: true,
        };
        let encoded = serde_json::to_value(command)?;
        assert_eq!(encoded["desired"], true);
        assert!(encoded.get("toggle").is_none());
        Ok(())
    }

    #[test]
    fn close_wire_model_never_claims_acceptance_or_implicit_replacement() {
        assert!(serde_json::from_str::<WindowCloseOutcome>("\"request_sent\"").is_ok());
        assert!(serde_json::from_str::<WindowCloseOutcome>("\"request_accepted\"").is_err());
        assert!(serde_json::from_str::<WindowCloseOutcome>("\"replaced\"").is_err());
    }

    #[test]
    fn converged_activation_rejects_focus_failure_warning() -> Result<(), WindowValidationError> {
        let requested = reference(12)?;
        let result = WindowActivateResult {
            requested: requested.clone(),
            observed_active: Some(requested.clone()),
            observed_focused: Some(requested),
            converged: true,
            warnings: vec![WindowControlWarning::FocusNotAcquired],
        };
        assert_eq!(
            result.validate(),
            Err(WindowControlValidationError::ObservedResult)
        );
        Ok(())
    }

    #[test]
    fn current_time_warning_is_valid_only_for_timestamped_operation_families()
    -> Result<(), Box<dyn std::error::Error>> {
        let requested = reference(13)?;
        WindowCloseResult {
            requested: requested.clone(),
            outcome: WindowCloseOutcome::RequestSent,
            final_snapshot: None,
            warnings: vec![WindowControlWarning::CurrentTimeFallback],
        }
        .validate()?;

        let final_snapshot = snapshot(requested.clone(), Vec::new())?;
        WindowStateResult {
            requested: requested.clone(),
            operation: WindowStateOperation::Minimized,
            desired: false,
            observed: WindowStateObservation::Disabled,
            converged: true,
            final_snapshot: final_snapshot.clone(),
            warnings: vec![WindowControlWarning::CurrentTimeFallback],
        }
        .validate()?;
        let impossible = WindowStateResult {
            requested,
            operation: WindowStateOperation::ManagerState {
                state: WindowManagerState::Above,
            },
            desired: false,
            observed: WindowStateObservation::Disabled,
            converged: true,
            final_snapshot,
            warnings: vec![WindowControlWarning::CurrentTimeFallback],
        };
        assert_eq!(
            impossible.validate(),
            Err(WindowControlValidationError::ObservedResult)
        );
        Ok(())
    }

    #[test]
    fn move_resize_requires_a_real_bounded_change() {
        let empty = WindowGeometryRequest {
            x: None,
            y: None,
            width: None,
            height: None,
        };
        assert_eq!(
            empty.validate(),
            Err(WindowControlValidationError::EmptyGeometry)
        );
        let oversized = WindowGeometryRequest {
            x: None,
            y: None,
            width: Some(MAX_WINDOW_DIMENSION + 1),
            height: None,
        };
        assert_eq!(
            oversized.validate(),
            Err(WindowControlValidationError::GeometryDimension)
        );
    }

    #[test]
    fn relative_stacking_requires_a_distinct_same_scope_sibling()
    -> Result<(), WindowValidationError> {
        let window = reference(11)?;
        let missing = WindowStackCommand {
            window: window.clone(),
            mode: WindowStackMode::Above,
            sibling: None,
        };
        assert_eq!(
            missing.validate(),
            Err(WindowControlValidationError::StackSibling)
        );

        let same = WindowStackCommand {
            window: window.clone(),
            mode: WindowStackMode::Below,
            sibling: Some(window),
        };
        assert_eq!(
            same.validate(),
            Err(WindowControlValidationError::StackSibling)
        );
        Ok(())
    }

    #[test]
    fn state_results_name_and_report_partial_compound_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = reference(21)?;
        let result = WindowStateResult {
            requested: window.clone(),
            operation: WindowStateOperation::ManagerState {
                state: WindowManagerState::Maximized,
            },
            desired: true,
            observed: WindowStateObservation::Partial,
            converged: false,
            final_snapshot: snapshot(
                window,
                vec![WindowAtomName::new("_NET_WM_STATE_MAXIMIZED_VERT")?],
            )?,
            warnings: vec![WindowControlWarning::PartialStateObserved],
        };
        result.validate()?;
        Ok(())
    }

    #[test]
    fn move_resize_result_binds_effective_and_observed_geometry()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = reference(31)?;
        let effective =
            WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 20, 100, 80)?)?;
        let geometry = WindowGeometry {
            client_rect: effective,
            frame_rect: None,
            content_rect: effective,
            frame_extents: None,
        };
        let result = WindowMoveResizeResult {
            requested: window,
            relative_to: WindowGeometryTarget::Client,
            desired: WindowGeometryRequest {
                x: Some(10),
                y: Some(20),
                width: Some(100),
                height: Some(80),
            },
            effective,
            observed: geometry,
            observed_revision: WindowModelRevision::new(4)?,
            constrained: false,
            converged: true,
            warnings: Vec::new(),
        };
        result.validate()?;
        Ok(())
    }

    #[test]
    fn move_resize_can_report_matching_but_not_quiet_geometry()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = reference(32)?;
        let effective =
            WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 20, 100, 80)?)?;
        let result = WindowMoveResizeResult {
            requested: window,
            relative_to: WindowGeometryTarget::Client,
            desired: WindowGeometryRequest {
                x: Some(10),
                y: Some(20),
                width: Some(100),
                height: Some(80),
            },
            effective,
            observed: WindowGeometry {
                client_rect: effective,
                frame_rect: None,
                content_rect: effective,
                frame_extents: None,
            },
            observed_revision: WindowModelRevision::new(5)?,
            constrained: false,
            converged: false,
            warnings: Vec::new(),
        };
        result.validate()?;
        Ok(())
    }

    #[test]
    fn move_resize_constraint_can_describe_clamped_omitted_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = reference(33)?;
        let effective =
            WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 30, 100, 80)?)?;
        let result = WindowMoveResizeResult {
            requested: window,
            relative_to: WindowGeometryTarget::Client,
            desired: WindowGeometryRequest {
                x: Some(10),
                y: None,
                width: None,
                height: None,
            },
            effective,
            observed: WindowGeometry {
                client_rect: effective,
                frame_rect: None,
                content_rect: effective,
                frame_extents: None,
            },
            observed_revision: WindowModelRevision::new(6)?,
            constrained: true,
            converged: true,
            warnings: vec![WindowControlWarning::GeometryConstrained],
        };
        result.validate()?;
        Ok(())
    }

    #[test]
    fn workspace_command_forbids_the_ewmh_all_desktops_sentinel()
    -> Result<(), WindowValidationError> {
        let command = WindowMoveToWorkspaceCommand {
            window: reference(35)?,
            workspace: u32::MAX,
        };
        assert_eq!(
            command.validate(),
            Err(WindowControlValidationError::Workspace)
        );
        Ok(())
    }

    #[test]
    fn workspace_result_derives_convergence_and_warning_from_observation()
    -> Result<(), Box<dyn std::error::Error>> {
        let requested = reference(36)?;
        WindowMoveToWorkspaceResult {
            requested: requested.clone(),
            desired_workspace: 2,
            observed_workspace: Some(2),
            observed_revision: WindowModelRevision::new(5)?,
            converged: true,
            warnings: Vec::new(),
        }
        .validate()?;

        WindowMoveToWorkspaceResult {
            requested: requested.clone(),
            desired_workspace: 2,
            observed_workspace: Some(1),
            observed_revision: WindowModelRevision::new(6)?,
            converged: false,
            warnings: vec![WindowControlWarning::WorkspaceNotConfirmed],
        }
        .validate()?;

        let contradictory = WindowMoveToWorkspaceResult {
            requested,
            desired_workspace: 2,
            observed_workspace: Some(1),
            observed_revision: WindowModelRevision::new(7)?,
            converged: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            contradictory.validate(),
            Err(WindowControlValidationError::ObservedResult)
        );
        Ok(())
    }

    #[test]
    fn relative_stack_result_derives_convergence_from_both_indices()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = reference(41)?;
        let sibling = WindowRef {
            xid: 42,
            ..window.clone()
        };
        let valid = WindowStackResult {
            requested: window,
            mode: WindowStackMode::Above,
            sibling: Some(sibling),
            observed_stacking_index: Some(2),
            observed_sibling_index: Some(1),
            observed_revision: WindowModelRevision::new(5)?,
            converged: true,
            warnings: Vec::new(),
        };
        valid.validate()?;
        Ok(())
    }

    #[test]
    fn wm_capabilities_reject_duplicates() -> Result<(), WindowValidationError> {
        let capabilities = WindowManagerCapabilities {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            model_revision: WindowModelRevision::new(1)?,
            supported: vec![
                WindowManagerCapability::Activate,
                WindowManagerCapability::Activate,
            ],
        };
        assert_eq!(
            capabilities.validate(),
            Err(WindowControlValidationError::Capabilities)
        );
        Ok(())
    }

    #[test]
    fn unknown_control_fields_are_rejected() {
        let value = serde_json::json!({
            "window": {
                "desktop_id": DesktopId::new(),
                "desktop_generation": DesktopGeneration::new(),
                "xid": 42,
                "observed_generation": "1",
                "identity_hash": "d".repeat(WINDOW_IDENTITY_HASH_BYTES)
            },
            "state": "above",
            "desired": true,
            "toggle": true
        });
        assert!(serde_json::from_value::<WindowSetStateCommand>(value).is_err());
    }
}
