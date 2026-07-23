//! Stable, safe problem-details types.

use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{DesktopGeneration, EffectStage, RequestId};

/// Maximum UTF-8 byte length of a problem type URI.
pub const MAX_PROBLEM_TYPE_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a problem title.
pub const MAX_PROBLEM_TITLE_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a safe problem detail.
pub const MAX_PROBLEM_DETAIL_BYTES: usize = 1_024;
/// Maximum UTF-8 byte length of an instance URI.
pub const MAX_PROBLEM_INSTANCE_BYTES: usize = 256;
/// Maximum number of extension members in safe problem details.
pub const MAX_PROBLEM_DETAILS: usize = 16;
/// Maximum UTF-8 byte length of one extension-member key.
pub const MAX_PROBLEM_DETAIL_KEY_BYTES: usize = 64;
/// Maximum encoded byte length of the complete extension details map.
pub const MAX_PROBLEM_DETAILS_ENCODED_BYTES: usize = 4_096;

/// A stable machine-readable error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Request syntax or shape is invalid.
    InvalidRequest,
    /// Protocol version negotiation failed.
    UnsupportedVersion,
    /// Authentication is missing or invalid.
    AuthenticationRequired,
    /// The principal lacks required authority.
    PermissionDenied,
    /// A referenced resource does not exist.
    NotFound,
    /// A reference points at an earlier generation or identity.
    StaleReference,
    /// A command identifier was reused with different content.
    CommandIdConflict,
    /// The controller lease is absent or conflicts.
    LeaseConflict,
    /// A bounded queue or quota is exhausted.
    ResourceExhausted,
    /// A required capability is not ready.
    CapabilityUnavailable,
    /// The target cannot perform this operation.
    UnsupportedByTarget,
    /// The application is absent from or unreachable through AT-SPI.
    ApplicationNotAccessible,
    /// The exact generation-fenced accessibility element no longer exists.
    ElementNotFound,
    /// A selector that required one target matched more than one.
    AmbiguousTarget,
    /// The referenced accessibility object reports or behaves as defunct.
    ElementDefunct,
    /// The target does not expose the required AT-SPI interface.
    InterfaceNotSupported,
    /// The element is not showing in the accessible hierarchy.
    ElementNotShowing,
    /// The element is not sensitive/enabled for the requested action.
    ElementNotSensitive,
    /// The element is not editable through the required semantic interface.
    ElementNotEditable,
    /// The requested semantic action name or index is absent.
    ActionNotFound,
    /// Element-to-X11-window correlation is below the required confidence.
    WeakWindowCorrelation,
    /// Accessible component geometry is missing, stale, or invalid.
    ElementGeometryInvalid,
    /// Occlusion policy rejected the resolved element click point.
    ElementOccluded,
    /// Accessibility traversal exhausted its declared query budget.
    QueryBudgetExceeded,
    /// A complete requested accessibility snapshot could not fit its budget.
    SnapshotTruncated,
    /// The toolkit returned malformed or contradictory AT-SPI data.
    ToolkitProtocolError,
    /// The semantic effect completed but its required postcondition failed.
    SemanticPostconditionFailed,
    /// Work exceeded its deadline before any effect.
    DeadlineExceededBeforeEffect,
    /// Work exceeded its deadline after a partial effect.
    DeadlineExceededAfterEffect,
    /// A transport deadline elapsed after dispatch, so the request outcome
    /// cannot be proven from the response channel.
    RequestOutcomeUnknown,
    /// Work was cancelled before any effect.
    CancelledBeforeEffect,
    /// Work was cancelled after a partial effect.
    CancelledAfterEffect,
    /// An external backend failed.
    BackendFailure,
    /// A server invariant failed. Details remain opaque to clients.
    Internal,
}

impl ErrorCode {
    /// Returns the stable snake-case wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedVersion => "unsupported_version",
            Self::AuthenticationRequired => "authentication_required",
            Self::PermissionDenied => "permission_denied",
            Self::NotFound => "not_found",
            Self::StaleReference => "stale_reference",
            Self::CommandIdConflict => "command_id_conflict",
            Self::LeaseConflict => "lease_conflict",
            Self::ResourceExhausted => "resource_exhausted",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::UnsupportedByTarget => "unsupported_by_target",
            Self::ApplicationNotAccessible => "application_not_accessible",
            Self::ElementNotFound => "element_not_found",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::ElementDefunct => "element_defunct",
            Self::InterfaceNotSupported => "interface_not_supported",
            Self::ElementNotShowing => "element_not_showing",
            Self::ElementNotSensitive => "element_not_sensitive",
            Self::ElementNotEditable => "element_not_editable",
            Self::ActionNotFound => "action_not_found",
            Self::WeakWindowCorrelation => "weak_window_correlation",
            Self::ElementGeometryInvalid => "element_geometry_invalid",
            Self::ElementOccluded => "element_occluded",
            Self::QueryBudgetExceeded => "query_budget_exceeded",
            Self::SnapshotTruncated => "snapshot_truncated",
            Self::ToolkitProtocolError => "toolkit_protocol_error",
            Self::SemanticPostconditionFailed => "semantic_postcondition_failed",
            Self::DeadlineExceededBeforeEffect => "deadline_exceeded_before_effect",
            Self::DeadlineExceededAfterEffect => "deadline_exceeded_after_effect",
            Self::RequestOutcomeUnknown => "request_outcome_unknown",
            Self::CancelledBeforeEffect => "cancelled_before_effect",
            Self::CancelledAfterEffect => "cancelled_after_effect",
            Self::BackendFailure => "backend_failure",
            Self::Internal => "internal",
        }
    }
}

/// Client retry guidance attached to a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    /// Repeating the operation is unsafe or cannot change the result.
    Never,
    /// Retrieve or resubmit the exact request with the same command identifier.
    SameCommandId,
    /// Refresh generation-bound state before creating a new command.
    AfterResync,
    /// No effect occurred; a new command may be attempted after delay.
    AfterBackoff,
}

/// RFC 9457-shaped safe problem details.
///
/// JSON Schema string lengths count Unicode code points; admission additionally
/// enforces the exported UTF-8 byte ceilings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Problem {
    #[serde(rename = "type")]
    #[schemars(length(min = 1, max = MAX_PROBLEM_TYPE_BYTES))]
    problem_type: String,
    #[schemars(length(min = 1, max = MAX_PROBLEM_TITLE_BYTES))]
    title: String,
    #[schemars(range(min = 400, max = 599))]
    status: u16,
    code: ErrorCode,
    #[schemars(length(min = 1, max = MAX_PROBLEM_DETAIL_BYTES))]
    detail: String,
    #[schemars(length(max = MAX_PROBLEM_INSTANCE_BYTES))]
    instance: Option<String>,
    retry: RetryAdvice,
    effect_stage: EffectStage,
    desktop_generation: Option<DesktopGeneration>,
    #[schemars(schema_with = "problem_details_schema")]
    details: BTreeMap<String, serde_json::Value>,
}

fn problem_details_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = generator.subschema_for::<BTreeMap<String, serde_json::Value>>();
    schema.insert(
        "maxProperties".to_owned(),
        serde_json::json!(MAX_PROBLEM_DETAILS),
    );
    schema.insert(
        "propertyNames".to_owned(),
        serde_json::json!({
            "minLength": 1,
            "maxLength": MAX_PROBLEM_DETAIL_KEY_BYTES,
            "pattern": "^[a-z0-9._-]+$"
        }),
    );
    schema
}

impl Problem {
    /// Creates checked, client-safe problem details.
    pub fn new(
        status: u16,
        code: ErrorCode,
        title: impl Into<String>,
        detail: impl Into<String>,
        retry: RetryAdvice,
        effect_stage: EffectStage,
    ) -> Result<Self, ProblemValidationError> {
        let title = title.into();
        let detail = detail.into();
        let problem = Self {
            problem_type: format!(
                "https://xenoteer.dev/problems/{}",
                code.as_str().replace('_', "-")
            ),
            title,
            status,
            code,
            detail,
            instance: None,
            retry,
            effect_stage,
            desktop_generation: None,
            details: BTreeMap::new(),
        };
        problem.validate()?;
        Ok(problem)
    }

    /// Attaches a request-scoped RFC 9457 instance URI.
    #[must_use]
    pub fn with_request(mut self, request_id: RequestId) -> Self {
        self.instance = Some(format!("urn:xenoteer:request:{request_id}"));
        self
    }

    /// Attaches the desktop generation involved in the failure.
    #[must_use]
    pub fn with_desktop_generation(mut self, generation: DesktopGeneration) -> Self {
        self.desktop_generation = Some(generation);
        self
    }

    /// Adds a bounded, pre-redacted detail value.
    pub fn with_detail(
        mut self,
        key: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<Self, ProblemValidationError> {
        let key = key.into();
        if key.is_empty()
            || key.len() > MAX_PROBLEM_DETAIL_KEY_BYTES
            || !key.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || self.details.len() >= MAX_PROBLEM_DETAILS
        {
            return Err(ProblemValidationError::DetailsLimit);
        }
        self.details.insert(key, value);
        self.validate()?;
        Ok(self)
    }

    /// Validates problem details obtained through deserialization.
    pub fn validate(&self) -> Result<(), ProblemValidationError> {
        if !(400..=599).contains(&self.status) {
            return Err(ProblemValidationError::InvalidStatus);
        }

        let expected_type = format!(
            "https://xenoteer.dev/problems/{}",
            self.code.as_str().replace('_', "-")
        );
        if self.problem_type != expected_type
            || self.problem_type.is_empty()
            || self.problem_type.len() > MAX_PROBLEM_TYPE_BYTES
        {
            return Err(ProblemValidationError::InvalidTypeUri);
        }

        if self.title.is_empty()
            || self.title.len() > MAX_PROBLEM_TITLE_BYTES
            || self.title.chars().any(char::is_control)
            || self.detail.is_empty()
            || self.detail.len() > MAX_PROBLEM_DETAIL_BYTES
            || self.detail.chars().any(char::is_control)
        {
            return Err(ProblemValidationError::InvalidTextLength);
        }

        if self.instance.as_ref().is_some_and(|instance| {
            instance.len() > MAX_PROBLEM_INSTANCE_BYTES || instance.chars().any(char::is_control)
        }) {
            return Err(ProblemValidationError::InvalidInstanceUri);
        }

        if matches!(
            self.retry,
            RetryAdvice::AfterBackoff | RetryAdvice::AfterResync
        ) && self.effect_stage.has_visible_effect()
        {
            return Err(ProblemValidationError::RetryEffectMismatch);
        }

        if self.details.len() > MAX_PROBLEM_DETAILS
            || self.details.keys().any(|key| {
                key.is_empty()
                    || key.len() > MAX_PROBLEM_DETAIL_KEY_BYTES
                    || !key.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'_' | b'-')
                    })
            })
            || serde_json::to_vec(&self.details)
                .map_err(|_| ProblemValidationError::DetailsLimit)?
                .len()
                > MAX_PROBLEM_DETAILS_ENCODED_BYTES
        {
            return Err(ProblemValidationError::DetailsLimit);
        }

        Ok(())
    }

    /// Returns the HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the machine-readable code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// Returns the effect stage reached before failure.
    #[must_use]
    pub const fn effect_stage(&self) -> EffectStage {
        self.effect_stage
    }
}

/// Problem-details validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProblemValidationError {
    /// HTTP problem statuses are restricted to 4xx and 5xx.
    #[error("problem status must be between 400 and 599")]
    InvalidStatus,
    /// The type URI is absent, excessive, or inconsistent with the error code.
    #[error("problem type URI is invalid")]
    InvalidTypeUri,
    /// Safe title or detail text is empty or too long.
    #[error("problem title or detail has invalid length")]
    InvalidTextLength,
    /// The request instance URI exceeds its safe public bound.
    #[error("problem instance URI is invalid")]
    InvalidInstanceUri,
    /// Retry guidance that creates new work was paired with a visible effect.
    #[error("problem retry advice is unsafe for its effect stage")]
    RetryEffectMismatch,
    /// The safe details map exceeds its count or encoded-byte budget.
    #[error("problem details exceed their bound")]
    DetailsLimit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_success_statuses() {
        assert_eq!(
            Problem::new(
                200,
                ErrorCode::Internal,
                "Internal error",
                "The operation failed.",
                RetryAdvice::Never,
                EffectStage::None,
            ),
            Err(ProblemValidationError::InvalidStatus)
        );
    }

    #[test]
    fn rejects_hostile_deserialized_problem() -> Result<(), Box<dyn std::error::Error>> {
        let valid = Problem::new(
            500,
            ErrorCode::Internal,
            "Internal error",
            "The operation failed.",
            RetryAdvice::Never,
            EffectStage::None,
        )?;
        let mut encoded = serde_json::to_value(valid)?;
        encoded["title"] = serde_json::Value::String("x".repeat(MAX_PROBLEM_TITLE_BYTES + 1));
        let decoded: Problem = serde_json::from_value(encoded)?;
        assert_eq!(
            decoded.validate(),
            Err(ProblemValidationError::InvalidTextLength)
        );
        Ok(())
    }

    #[test]
    fn type_uri_must_match_error_code() -> Result<(), Box<dyn std::error::Error>> {
        let valid = Problem::new(
            500,
            ErrorCode::Internal,
            "Internal error",
            "The operation failed.",
            RetryAdvice::Never,
            EffectStage::None,
        )?;
        let mut encoded = serde_json::to_value(valid)?;
        encoded["type"] = serde_json::json!("https://attacker.invalid/problem");
        let decoded: Problem = serde_json::from_value(encoded)?;
        assert_eq!(
            decoded.validate(),
            Err(ProblemValidationError::InvalidTypeUri)
        );
        Ok(())
    }

    #[test]
    fn new_command_retry_advice_requires_no_visible_effect() {
        assert_eq!(
            Problem::new(
                503,
                ErrorCode::BackendFailure,
                "Backend unavailable",
                "The backend is temporarily unavailable.",
                RetryAdvice::AfterBackoff,
                EffectStage::ButtonPressed,
            ),
            Err(ProblemValidationError::RetryEffectMismatch)
        );
        assert_eq!(
            Problem::new(
                409,
                ErrorCode::StaleReference,
                "Reference is stale",
                "Refresh desktop state before retrying.",
                RetryAdvice::AfterResync,
                EffectStage::PointerMoved,
            ),
            Err(ProblemValidationError::RetryEffectMismatch)
        );
    }

    #[test]
    fn deadline_error_codes_use_catalog_wire_names() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_string(&ErrorCode::DeadlineExceededBeforeEffect)?,
            r#""deadline_exceeded_before_effect""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::DeadlineExceededAfterEffect)?,
            r#""deadline_exceeded_after_effect""#
        );
        Ok(())
    }

    #[test]
    fn accessibility_error_codes_use_stable_catalog_wire_names() -> Result<(), serde_json::Error> {
        let cases = [
            (
                ErrorCode::ApplicationNotAccessible,
                "application_not_accessible",
            ),
            (ErrorCode::ElementNotFound, "element_not_found"),
            (ErrorCode::AmbiguousTarget, "ambiguous_target"),
            (ErrorCode::ElementDefunct, "element_defunct"),
            (ErrorCode::InterfaceNotSupported, "interface_not_supported"),
            (ErrorCode::ElementNotShowing, "element_not_showing"),
            (ErrorCode::ElementNotSensitive, "element_not_sensitive"),
            (ErrorCode::ElementNotEditable, "element_not_editable"),
            (ErrorCode::ActionNotFound, "action_not_found"),
            (ErrorCode::WeakWindowCorrelation, "weak_window_correlation"),
            (
                ErrorCode::ElementGeometryInvalid,
                "element_geometry_invalid",
            ),
            (ErrorCode::ElementOccluded, "element_occluded"),
            (ErrorCode::QueryBudgetExceeded, "query_budget_exceeded"),
            (ErrorCode::SnapshotTruncated, "snapshot_truncated"),
            (ErrorCode::ToolkitProtocolError, "toolkit_protocol_error"),
            (
                ErrorCode::SemanticPostconditionFailed,
                "semantic_postcondition_failed",
            ),
        ];
        for (code, wire) in cases {
            assert_eq!(code.as_str(), wire);
            assert_eq!(serde_json::to_string(&code)?, format!("\"{wire}\""));
        }
        Ok(())
    }
}
