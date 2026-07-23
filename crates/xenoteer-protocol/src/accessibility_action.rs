//! Strict semantic-action and physical-element-click command contracts.

#![allow(missing_docs)]

use core::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::accessibility::{
    AccessibilityRevision, AccessibilityValidationError, ElementSnapshot, ElementState,
    ElementWaitPredicate, MAX_ACCESSIBILITY_ACTIONS, MAX_ACCESSIBILITY_SHORT_TEXT_BYTES,
    MAX_ACCESSIBILITY_TEXT_BYTES, StrictElementRef, deserialize_strict_element_ref,
    validate_element_scope,
};
use crate::geometry::{StrictPoint, deserialize_strict_point};
use crate::window::{StrictWindowRef, deserialize_optional_strict_window_ref};
use crate::{
    DEFAULT_DOUBLE_CLICK_THRESHOLD_MS, DesktopGeneration, DesktopId, ElementRef,
    MAX_POINTER_CLICK_COUNT, MAX_POINTER_MOVE_DURATION_MS, Point, PointerCurve,
    PointerLogicalButton, Rect, WindowCorrelationConfidence, WindowRef,
};

pub const MAX_SEMANTIC_POSTCONDITION_TIMEOUT_MS: u32 = 30_000;
pub const MAX_EDITABLE_TEXT_OFFSET: i32 = i32::MAX - 1;
pub const MAX_PHYSICAL_ELEMENT_CLICK_COUNT: u8 = MAX_POINTER_CLICK_COUNT;
pub const MAX_PHYSICAL_ELEMENT_CLICK_INTERVAL_MS: u32 =
    DEFAULT_DOUBLE_CLICK_THRESHOLD_MS as u32 - 1;
pub const MAX_ELEMENT_CLICK_SETTLE_TIMEOUT_MS: u32 = 30_000;

#[derive(Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct SemanticTextInput(String);

impl SemanticTextInput {
    pub fn new(value: impl Into<String>) -> Result<Self, AccessibilityActionValidationError> {
        let value = value.into();
        if value.len() > MAX_ACCESSIBILITY_TEXT_BYTES || value.contains('\0') {
            return Err(AccessibilityActionValidationError::Text);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SemanticTextInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SemanticTextInput(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for SemanticTextInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementPostcondition {
    pub predicate: ElementWaitPredicate,
    #[schemars(range(min = 1, max = MAX_SEMANTIC_POSTCONDITION_TIMEOUT_MS))]
    pub timeout_ms: u32,
    pub allow_poll_fallback: bool,
}

impl ElementPostcondition {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.predicate.validate()?;
        if self.timeout_ms == 0 || self.timeout_ms > MAX_SEMANTIC_POSTCONDITION_TIMEOUT_MS {
            return Err(AccessibilityActionValidationError::PostconditionTimeout);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementActionTarget {
    Name {
        name: String,
    },
    Index {
        #[schemars(range(max = MAX_ACCESSIBILITY_ACTIONS - 1))]
        index: u16,
    },
    Default,
}

impl ElementActionTarget {
    fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        match self {
            Self::Name { name }
                if name.is_empty()
                    || name.len() > MAX_ACCESSIBILITY_SHORT_TEXT_BYTES
                    || name.contains('\0') =>
            {
                Err(AccessibilityActionValidationError::ActionName)
            }
            Self::Index { index } if usize::from(*index) >= MAX_ACCESSIBILITY_ACTIONS => {
                Err(AccessibilityActionValidationError::ActionIndex)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementInvokeCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub action: ElementActionTarget,
    pub allow_disabled: bool,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementInvokeCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        self.action.validate()?;
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementFocusCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub require_window_focus_correlation: bool,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementFocusCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSetValueCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub value: f64,
    pub tolerance: Option<f64>,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementSetValueCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        if !self.value.is_finite()
            || self
                .tolerance
                .is_some_and(|tolerance| !tolerance.is_finite() || tolerance < 0.0)
        {
            return Err(AccessibilityActionValidationError::Value);
        }
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementSelectionOperation {
    SelectChild { index: u32 },
    DeselectChild { index: u32 },
    SelectAll,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSelectionCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub operation: ElementSelectionOperation,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementSelectionCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditableTextSelectionPolicy {
    Preserve,
    CollapseBefore,
    CollapseAfter,
    SelectInserted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSetTextCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub text: SemanticTextInput,
    pub selection: EditableTextSelectionPolicy,
    pub verify_length_only: bool,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementSetTextCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementInsertTextCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    #[schemars(range(min = 0, max = MAX_EDITABLE_TEXT_OFFSET))]
    pub offset: i32,
    pub text: SemanticTextInput,
    pub selection: EditableTextSelectionPolicy,
    pub verify_length_only: bool,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementInsertTextCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        if self.offset < 0 || self.offset > MAX_EDITABLE_TEXT_OFFSET {
            return Err(AccessibilityActionValidationError::TextOffset);
        }
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementScrollAlignment {
    TopLeft,
    BottomRight,
    TopEdge,
    BottomEdge,
    LeftEdge,
    RightEdge,
    Anywhere,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementScrollTarget {
    Alignment {
        alignment: ElementScrollAlignment,
    },
    ScreenPoint {
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        point: Point,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementScrollCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    pub target: ElementScrollTarget,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementScrollCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        validate_postcondition(&self.postcondition)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementClickPointPolicy {
    Center,
    InsetCenter {
        inset_pixels: u16,
    },
    Offset {
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        offset: Point,
    },
    NearestVisible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementClickScrollPolicy {
    Never,
    IfNeeded,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementOcclusionPolicy {
    Ignore,
    BestEffortReject,
    RequireUnoccluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementWindowActivationPolicy {
    Never,
    IfNeeded,
    Require,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementPhysicalClickCommand {
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    #[serde(deserialize_with = "deserialize_optional_strict_window_ref")]
    #[schemars(with = "Option<StrictWindowRef>")]
    pub window: Option<WindowRef>,
    pub minimum_correlation: WindowCorrelationConfidence,
    pub point_policy: ElementClickPointPolicy,
    pub scroll_policy: ElementClickScrollPolicy,
    pub activation_policy: ElementWindowActivationPolicy,
    pub occlusion_policy: ElementOcclusionPolicy,
    pub button: PointerLogicalButton,
    #[schemars(range(min = 1, max = MAX_PHYSICAL_ELEMENT_CLICK_COUNT))]
    pub count: u8,
    #[schemars(range(max = MAX_PHYSICAL_ELEMENT_CLICK_INTERVAL_MS))]
    pub interval_ms: u32,
    pub move_duration_ms: Option<u32>,
    pub curve: PointerCurve,
    #[schemars(range(min = 1, max = MAX_ELEMENT_CLICK_SETTLE_TIMEOUT_MS))]
    pub settle_timeout_ms: u32,
    pub postcondition: Option<ElementPostcondition>,
}

impl ElementPhysicalClickCommand {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        if let Some(window) = &self.window {
            window
                .validate_shape()
                .map_err(|_| AccessibilityActionValidationError::ReferenceScope)?;
            if window.desktop_id != self.element.desktop_id
                || window.desktop_generation != self.element.desktop_generation
            {
                return Err(AccessibilityActionValidationError::ReferenceScope);
            }
        }
        if self.minimum_correlation == WindowCorrelationConfidence::None
            || (self.minimum_correlation == WindowCorrelationConfidence::Weak
                && self.window.is_none())
            || self.count == 0
            || self.count > MAX_PHYSICAL_ELEMENT_CLICK_COUNT
            || self.interval_ms > MAX_PHYSICAL_ELEMENT_CLICK_INTERVAL_MS
            || self
                .move_duration_ms
                .is_some_and(|duration| duration == 0 || duration > MAX_POINTER_MOVE_DURATION_MS)
            || self.settle_timeout_ms == 0
            || self.settle_timeout_ms > MAX_ELEMENT_CLICK_SETTLE_TIMEOUT_MS
        {
            return Err(AccessibilityActionValidationError::PhysicalClick);
        }
        if self.curve == PointerCurve::Instant {
            return Err(AccessibilityActionValidationError::PhysicalClick);
        }
        validate_postcondition(&self.postcondition)
    }
}

pub(crate) fn validate_command_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    element: &ElementRef,
) -> Result<(), AccessibilityActionValidationError> {
    validate_element_scope(desktop_id, desktop_generation, element).map_err(Into::into)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementActionOperation {
    Invoke,
    Focus,
    SetValue,
    Selection,
    SetText,
    InsertText,
    Scroll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementActionEvidence {
    pub resolved_action_name: Option<String>,
    pub resolved_action_index: Option<u16>,
    pub backend_accepted: bool,
    pub observed_state: Option<ElementState>,
    pub observed_value: Option<f64>,
    pub observed_selection_count: Option<u32>,
    pub observed_text_length: Option<u32>,
    pub protected_text_verified_by_length_only: bool,
    pub extents_before: Option<Rect>,
    pub extents_after: Option<Rect>,
    pub postcondition_satisfied: Option<bool>,
    pub poll_fallback_used: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementActionResult {
    pub operation: ElementActionOperation,
    pub element: ElementRef,
    pub revision_before: AccessibilityRevision,
    pub revision_after: AccessibilityRevision,
    pub snapshot_before: Option<Box<ElementSnapshot>>,
    pub snapshot_after: Option<Box<ElementSnapshot>>,
    pub evidence: ElementActionEvidence,
}

impl ElementActionResult {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        if self.revision_after < self.revision_before
            || !self.evidence.backend_accepted
            || self
                .evidence
                .observed_value
                .is_some_and(|value| !value.is_finite())
            || self
                .evidence
                .extents_before
                .is_some_and(|rect| rect.validate().is_err())
            || self
                .evidence
                .extents_after
                .is_some_and(|rect| rect.validate().is_err())
            || self.evidence.postcondition_satisfied == Some(false)
        {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        if let Some(name) = &self.evidence.resolved_action_name
            && (name.is_empty()
                || name.len() > MAX_ACCESSIBILITY_SHORT_TEXT_BYTES
                || name.contains('\0'))
        {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        if self
            .evidence
            .resolved_action_index
            .is_some_and(|index| usize::from(index) >= MAX_ACCESSIBILITY_ACTIONS)
        {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        let has_action_resolution = self.evidence.resolved_action_name.is_some()
            && self.evidence.resolved_action_index.is_some();
        if (self.operation == ElementActionOperation::Invoke) != has_action_resolution {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        if let Some(snapshot) = &self.snapshot_before {
            snapshot.validate()?;
            validate_same_element_scope(&self.element, &snapshot.element)?;
        }
        if let Some(snapshot) = &self.snapshot_after {
            snapshot.validate()?;
            validate_same_element_scope(&self.element, &snapshot.element)?;
        }
        if self.evidence.protected_text_verified_by_length_only
            && self.evidence.observed_text_length.is_none()
        {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OcclusionCheckResult {
    NotRequested,
    Clear,
    Occluded,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementPhysicalClickResult {
    pub element: ElementRef,
    pub window: WindowRef,
    pub correlation: WindowCorrelationConfidence,
    pub revision_before_queue: AccessibilityRevision,
    pub revision_after_queue: AccessibilityRevision,
    pub extents_before_queue: Rect,
    pub extents_after_queue: Rect,
    pub click_point: Point,
    pub occlusion_check: OcclusionCheckResult,
    pub scrolled: bool,
    pub window_activated: bool,
    pub pointer_interpolated: bool,
    pub button: PointerLogicalButton,
    pub count: u8,
    pub postcondition_satisfied: Option<bool>,
    pub final_snapshot: Option<Box<ElementSnapshot>>,
}

impl ElementPhysicalClickResult {
    pub fn validate(&self) -> Result<(), AccessibilityActionValidationError> {
        self.element.validate()?;
        self.window
            .validate_shape()
            .map_err(|_| AccessibilityActionValidationError::ReferenceScope)?;
        if self.window.desktop_id != self.element.desktop_id
            || self.window.desktop_generation != self.element.desktop_generation
            || !matches!(
                self.correlation,
                WindowCorrelationConfidence::Strong | WindowCorrelationConfidence::ExactProcess
            )
            || self.count == 0
            || self.count > MAX_PHYSICAL_ELEMENT_CLICK_COUNT
            || self.revision_after_queue < self.revision_before_queue
            || self.extents_before_queue.validate().is_err()
            || self.extents_after_queue.validate().is_err()
            || !self.pointer_interpolated
            || self.postcondition_satisfied == Some(false)
        {
            return Err(AccessibilityActionValidationError::ResultEvidence);
        }
        if let Some(snapshot) = &self.final_snapshot {
            snapshot.validate()?;
            validate_same_element_scope(&self.element, &snapshot.element)?;
        }
        Ok(())
    }
}

fn validate_postcondition(
    postcondition: &Option<ElementPostcondition>,
) -> Result<(), AccessibilityActionValidationError> {
    if let Some(postcondition) = postcondition {
        postcondition.validate()?;
    }
    Ok(())
}

fn validate_same_element_scope(
    expected: &ElementRef,
    actual: &ElementRef,
) -> Result<(), AccessibilityActionValidationError> {
    if expected.desktop_id != actual.desktop_id
        || expected.desktop_generation != actual.desktop_generation
        || expected.atspi_generation != actual.atspi_generation
        || expected.application != actual.application
        || expected.object_path != actual.object_path
        || expected.object_identity_hash != actual.object_identity_hash
        || expected.cache_sequence != actual.cache_sequence
    {
        return Err(AccessibilityActionValidationError::ReferenceScope);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AccessibilityActionValidationError {
    #[error("accessibility reference is invalid: {0}")]
    Accessibility(#[from] AccessibilityValidationError),
    #[error("semantic action name is invalid")]
    ActionName,
    #[error("semantic action index is invalid")]
    ActionIndex,
    #[error("semantic postcondition timeout is invalid")]
    PostconditionTimeout,
    #[error("semantic value or tolerance is invalid")]
    Value,
    #[error("editable text payload is invalid")]
    Text,
    #[error("editable text offset is invalid")]
    TextOffset,
    #[error("physical element click options are invalid")]
    PhysicalClick,
    #[error("command reference belongs to another desktop lifetime")]
    ReferenceScope,
    #[error("action result evidence is inconsistent")]
    ResultEvidence,
}
