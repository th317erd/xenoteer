//! Command lifecycle, effect, and result types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ClipboardValidationError, CommandId, ErrorCode, Problem, ProblemValidationError, ProcessRef,
    ProcessState, ProcessValidationError, ProcessView, TextInsertEvidence, Timestamp,
    TimestampError, WindowControlResult, WindowControlValidationError,
};

/// Maximum UTF-8 byte length of a warning code.
pub const MAX_WARNING_CODE_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a safe warning message.
pub const MAX_WARNING_MESSAGE_BYTES: usize = 512;
/// Maximum number of warnings attached to one result.
pub const MAX_RESULT_WARNINGS: usize = 16;

/// Lifecycle state of one deduplicated command execution.
///
/// Deadline lifecycle wire names remain `deadline_before_effect` and
/// `deadline_after_effect`; problem error codes use the fuller
/// `deadline_exceeded_*` catalog names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandLifecycle {
    /// Admitted to the result ledger but not yet executing.
    Accepted,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Completed with a non-cancellation failure.
    Failed,
    /// Cancelled without an externally visible effect.
    CancelledBeforeEffect,
    /// Cancelled after an externally visible effect.
    CancelledAfterEffect,
    /// Deadline elapsed without an externally visible effect.
    DeadlineBeforeEffect,
    /// Deadline elapsed after an externally visible effect.
    DeadlineAfterEffect,
}

impl CommandLifecycle {
    /// Returns whether no further lifecycle transition is permitted.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Accepted | Self::Running)
    }
}

/// Last externally visible stage reached by a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectStage {
    /// No effect occurred.
    None,
    /// The command was admitted but has not touched the target.
    Accepted,
    /// Transport dispatch began, but the server cannot prove whether a target
    /// effect occurred. Clients must not create fresh work automatically.
    OutcomeUnknown,
    /// A side effect occurred but the failing backend could not safely prove a
    /// more specific stage. Clients must treat automatic retry as unsafe.
    SideEffectObserved,
    /// The pointer moved.
    PointerMoved,
    /// A pointer button was pressed.
    ButtonPressed,
    /// A pointer button was released.
    ButtonReleased,
    /// One or more complete pointer clicks were confirmed.
    PointerClicked,
    /// A complete press/move/release drag was confirmed.
    PointerDragged,
    /// One or more complete discrete scroll notches were confirmed.
    PointerScrolled,
    /// A physical key was pressed.
    KeyPressed,
    /// A physical key was released.
    KeyReleased,
    /// A complete key press, chord, or sequence was confirmed.
    KeyboardActionCompleted,
    /// Xenoteer-owned pressed input was conservatively reset.
    InputReset,
    /// A managed application child was started.
    ProcessStarted,
    /// A managed process group received a termination signal.
    ProcessSignalled,
    /// A managed child was awaited and reaped.
    ProcessExited,
    /// The requested postcondition was observed.
    PostconditionMet,
    /// A window-manager or ICCCM request crossed its externally visible boundary.
    WindowRequestSent,
    /// The requested window postcondition was observed.
    WindowStateChanged,
    /// Xenoteer acquired or relinquished X11 selection ownership.
    ClipboardOwnershipChanged,
    /// At least one text-insertion effect was emitted.
    TextInserted,
}

impl EffectStage {
    /// Returns whether this stage represents an externally visible target effect.
    #[must_use]
    pub const fn has_visible_effect(self) -> bool {
        !matches!(self, Self::None | Self::Accepted)
    }
}

/// A successful command payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// A Phase-0 backend probe acknowledgment.
    Probe {
        /// Whether the probed capability was ready at execution time.
        ready: bool,
    },
    /// A registered application profile launched successfully.
    ApplicationLaunched {
        /// PID-reuse-safe managed process identity.
        process: ProcessRef,
    },
    /// Current managed process status.
    ProcessStatus {
        /// Validated process status snapshot.
        process: ProcessView,
    },
    /// Managed termination completed and the child was reaped.
    ProcessTerminated {
        /// Terminal process status snapshot.
        process: ProcessView,
    },
    /// An operation completed without an additional payload.
    Acknowledged,
    /// A window operation completed with bounded observed evidence.
    WindowControl {
        /// Desired operation and observed postcondition.
        result: WindowControlResult,
    },
    /// Text insertion completed with content-free delivery evidence.
    TextInserted {
        /// Selected strategy and bounded completed counts.
        evidence: TextInsertEvidence,
    },
}

impl CommandOutcome {
    /// Revalidates nested process references and state-dependent outcomes.
    pub fn validate(&self) -> Result<(), ResultInvariantError> {
        match self {
            Self::Probe { .. } | Self::Acknowledged => Ok(()),
            Self::ApplicationLaunched { process } => process.validate().map_err(Into::into),
            Self::ProcessStatus { process } => process.validate().map_err(Into::into),
            Self::ProcessTerminated { process } => {
                process.validate()?;
                if process.state != ProcessState::Exited {
                    return Err(ProcessValidationError::ProcessView.into());
                }
                Ok(())
            }
            Self::WindowControl { result } => result.validate().map_err(Into::into),
            Self::TextInserted { evidence } => evidence.validate().map_err(Into::into),
        }
    }
}

/// A bounded safe warning returned with a result.
///
/// JSON Schema string lengths count Unicode code points; admission additionally
/// enforces the public UTF-8 byte ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Warning {
    #[schemars(
        length(min = 1, max = MAX_WARNING_CODE_BYTES),
        regex(pattern = "^[a-z0-9._-]+$")
    )]
    code: String,
    #[schemars(length(min = 1, max = MAX_WARNING_MESSAGE_BYTES))]
    message: String,
}

impl Warning {
    /// Creates a checked stable warning.
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, WarningValidationError> {
        let warning = Self {
            code: code.into(),
            message: message.into(),
        };
        warning.validate()?;
        Ok(warning)
    }

    /// Validates a warning obtained through deserialization.
    pub fn validate(&self) -> Result<(), WarningValidationError> {
        if self.code.is_empty()
            || self.code.len() > MAX_WARNING_CODE_BYTES
            || !self.code.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        {
            return Err(WarningValidationError::Code);
        }
        if self.message.is_empty()
            || self.message.len() > MAX_WARNING_MESSAGE_BYTES
            || self.message.chars().any(char::is_control)
        {
            return Err(WarningValidationError::Message);
        }
        Ok(())
    }

    /// Returns the stable warning code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the safe human-readable warning message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Warning output validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WarningValidationError {
    /// Warning code is empty, too long, or not stable lowercase ASCII.
    #[error("warning code is invalid")]
    Code,
    /// Warning message is empty, too long, or contains control characters.
    #[error("warning message is invalid")]
    Message,
}

/// State and terminal payload for one command identifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandResult {
    command_id: CommandId,
    lifecycle: CommandLifecycle,
    effect_stage: EffectStage,
    accepted_at: Timestamp,
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
    outcome: Option<CommandOutcome>,
    error: Option<Problem>,
    #[schemars(length(max = MAX_RESULT_WARNINGS))]
    warnings: Vec<Warning>,
}

impl CommandResult {
    /// Creates the initial accepted result-ledger entry.
    #[must_use]
    pub fn accepted(command_id: CommandId, accepted_at: Timestamp) -> Self {
        Self {
            command_id,
            lifecycle: CommandLifecycle::Accepted,
            effect_stage: EffectStage::Accepted,
            accepted_at,
            started_at: None,
            finished_at: None,
            outcome: None,
            error: None,
            warnings: Vec::new(),
        }
    }

    /// Transitions an accepted result to running.
    pub fn start(mut self, started_at: Timestamp) -> Result<Self, ResultInvariantError> {
        if self.lifecycle != CommandLifecycle::Accepted {
            return Err(ResultInvariantError::IllegalTransition);
        }
        self.lifecycle = CommandLifecycle::Running;
        self.started_at = Some(started_at);
        self.validate()?;
        Ok(self)
    }

    /// Completes a running command successfully.
    pub fn succeed(
        mut self,
        effect_stage: EffectStage,
        outcome: CommandOutcome,
        finished_at: Timestamp,
    ) -> Result<Self, ResultInvariantError> {
        if self.lifecycle != CommandLifecycle::Running {
            return Err(ResultInvariantError::IllegalTransition);
        }
        self.lifecycle = CommandLifecycle::Succeeded;
        self.effect_stage = effect_stage;
        self.finished_at = Some(finished_at);
        self.outcome = Some(outcome);
        self.validate()?;
        Ok(self)
    }

    /// Completes an accepted or running command with a failure lifecycle.
    pub fn fail(
        mut self,
        lifecycle: CommandLifecycle,
        problem: Problem,
        finished_at: Timestamp,
    ) -> Result<Self, ResultInvariantError> {
        let legal_transition = matches!(
            (self.lifecycle, lifecycle),
            (
                CommandLifecycle::Accepted,
                CommandLifecycle::CancelledBeforeEffect | CommandLifecycle::DeadlineBeforeEffect
            ) | (
                CommandLifecycle::Running,
                CommandLifecycle::Failed
                    | CommandLifecycle::CancelledBeforeEffect
                    | CommandLifecycle::CancelledAfterEffect
                    | CommandLifecycle::DeadlineBeforeEffect
                    | CommandLifecycle::DeadlineAfterEffect
            )
        );
        if !legal_transition {
            return Err(ResultInvariantError::IllegalTransition);
        }
        self.lifecycle = lifecycle;
        self.effect_stage = problem.effect_stage();
        self.finished_at = Some(finished_at);
        self.error = Some(problem);
        self.validate()?;
        Ok(self)
    }

    /// Adds a pre-bounded warning.
    pub fn add_warning(&mut self, warning: Warning) -> Result<(), ResultInvariantError> {
        if self.warnings.len() >= MAX_RESULT_WARNINGS {
            return Err(ResultInvariantError::WarningLimit);
        }
        warning.validate()?;
        self.warnings.push(warning);
        Ok(())
    }

    /// Validates an instance obtained through deserialization.
    pub fn validate(&self) -> Result<(), ResultInvariantError> {
        let valid_payload = match self.lifecycle {
            CommandLifecycle::Accepted => {
                self.started_at.is_none()
                    && self.finished_at.is_none()
                    && self.outcome.is_none()
                    && self.error.is_none()
            }
            CommandLifecycle::Running => {
                self.started_at.is_some()
                    && self.finished_at.is_none()
                    && self.outcome.is_none()
                    && self.error.is_none()
            }
            CommandLifecycle::Succeeded => {
                self.started_at.is_some()
                    && self.finished_at.is_some()
                    && self.outcome.is_some()
                    && self.error.is_none()
            }
            CommandLifecycle::Failed
            | CommandLifecycle::CancelledAfterEffect
            | CommandLifecycle::DeadlineAfterEffect => {
                self.started_at.is_some()
                    && self.finished_at.is_some()
                    && self.outcome.is_none()
                    && self.error.is_some()
            }
            CommandLifecycle::CancelledBeforeEffect | CommandLifecycle::DeadlineBeforeEffect => {
                self.finished_at.is_some() && self.outcome.is_none() && self.error.is_some()
            }
        };
        if !valid_payload {
            return Err(ResultInvariantError::PayloadDoesNotMatchLifecycle);
        }
        if let Some(outcome) = &self.outcome {
            outcome.validate()?;
        }
        let effect_matches_lifecycle = match self.lifecycle {
            CommandLifecycle::Accepted => self.effect_stage == EffectStage::Accepted,
            CommandLifecycle::CancelledBeforeEffect | CommandLifecycle::DeadlineBeforeEffect => {
                !self.effect_stage.has_visible_effect()
            }
            CommandLifecycle::CancelledAfterEffect | CommandLifecycle::DeadlineAfterEffect => {
                self.effect_stage.has_visible_effect()
            }
            CommandLifecycle::Running | CommandLifecycle::Succeeded | CommandLifecycle::Failed => {
                true
            }
        };
        if !effect_matches_lifecycle {
            return Err(ResultInvariantError::EffectDoesNotMatchLifecycle);
        }

        if let Some(problem) = &self.error {
            problem.validate()?;
            if problem.effect_stage() != self.effect_stage {
                return Err(ResultInvariantError::ProblemEffectMismatch);
            }
            let specialized_code_matches = match self.lifecycle {
                CommandLifecycle::CancelledBeforeEffect => {
                    problem.code() == ErrorCode::CancelledBeforeEffect
                }
                CommandLifecycle::CancelledAfterEffect => {
                    problem.code() == ErrorCode::CancelledAfterEffect
                }
                CommandLifecycle::DeadlineBeforeEffect => {
                    problem.code() == ErrorCode::DeadlineExceededBeforeEffect
                }
                CommandLifecycle::DeadlineAfterEffect => {
                    problem.code() == ErrorCode::DeadlineExceededAfterEffect
                }
                CommandLifecycle::Failed => !matches!(
                    problem.code(),
                    ErrorCode::CancelledBeforeEffect
                        | ErrorCode::CancelledAfterEffect
                        | ErrorCode::DeadlineExceededBeforeEffect
                        | ErrorCode::DeadlineExceededAfterEffect
                ),
                CommandLifecycle::Accepted
                | CommandLifecycle::Running
                | CommandLifecycle::Succeeded => true,
            };
            if !specialized_code_matches {
                return Err(ResultInvariantError::ProblemCodeMismatch);
            }
        }

        let accepted_at = self.accepted_at.unix_timestamp_nanos()?;
        let started_at = self
            .started_at
            .as_ref()
            .map(Timestamp::unix_timestamp_nanos)
            .transpose()?;
        let finished_at = self
            .finished_at
            .as_ref()
            .map(Timestamp::unix_timestamp_nanos)
            .transpose()?;
        if started_at.is_some_and(|started| started < accepted_at)
            || finished_at.is_some_and(|finished| finished < accepted_at)
            || started_at
                .zip(finished_at)
                .is_some_and(|(started, finished)| finished < started)
        {
            return Err(ResultInvariantError::TimestampOrder);
        }

        if self.warnings.len() > MAX_RESULT_WARNINGS {
            return Err(ResultInvariantError::WarningLimit);
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        Ok(())
    }

    /// Returns the command identifier.
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Returns the current lifecycle.
    #[must_use]
    pub const fn lifecycle(&self) -> CommandLifecycle {
        self.lifecycle
    }

    /// Returns the current effect stage.
    #[must_use]
    pub const fn effect_stage(&self) -> EffectStage {
        self.effect_stage
    }

    /// Returns the successful payload when terminal success was reached.
    #[must_use]
    pub const fn outcome(&self) -> Option<&CommandOutcome> {
        self.outcome.as_ref()
    }

    /// Returns the safe problem when terminal failure was reached.
    #[must_use]
    pub const fn error(&self) -> Option<&Problem> {
        self.error.as_ref()
    }
}

/// A command-result construction or transition violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResultInvariantError {
    /// The requested lifecycle transition is not permitted.
    #[error("illegal command-result lifecycle transition")]
    IllegalTransition,
    /// Terminal/non-terminal payload fields do not match the lifecycle.
    #[error("command-result payload does not match lifecycle")]
    PayloadDoesNotMatchLifecycle,
    /// The warning count or content exceeds its bound.
    #[error("command-result warning limit exceeded")]
    WarningLimit,
    /// One warning contains invalid bounded public output.
    #[error(transparent)]
    InvalidWarning(#[from] WarningValidationError),
    /// A nested problem contains invalid bounded public output.
    #[error(transparent)]
    InvalidProblem(#[from] ProblemValidationError),
    /// The result effect stage is inconsistent with its lifecycle.
    #[error("command-result effect stage does not match lifecycle")]
    EffectDoesNotMatchLifecycle,
    /// A failure result disagrees with its nested problem about the effect stage.
    #[error("command-result and problem effect stages differ")]
    ProblemEffectMismatch,
    /// A specialized terminal lifecycle disagrees with the nested error code.
    #[error("command-result lifecycle and problem error code differ")]
    ProblemCodeMismatch,
    /// A lifecycle timestamp precedes an earlier lifecycle event.
    #[error("command-result timestamps are not monotonic")]
    TimestampOrder,
    /// An embedded timestamp could not be interpreted.
    #[error(transparent)]
    InvalidTimestamp(#[from] TimestampError),
    /// A nested managed-process outcome is malformed.
    #[error(transparent)]
    InvalidProcess(#[from] ProcessValidationError),
    /// A nested window-control outcome is malformed.
    #[error(transparent)]
    InvalidWindowControl(#[from] WindowControlValidationError),
    /// A nested clipboard/text outcome is malformed.
    #[error(transparent)]
    InvalidClipboard(#[from] ClipboardValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RetryAdvice;

    fn problem(
        code: ErrorCode,
        effect_stage: EffectStage,
    ) -> Result<Problem, ProblemValidationError> {
        Problem::new(
            500,
            code,
            "Internal error",
            "The operation failed.",
            RetryAdvice::Never,
            effect_stage,
        )
    }

    #[test]
    fn constructors_preserve_terminal_payload_invariant() -> Result<(), Box<dyn std::error::Error>>
    {
        let accepted_at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let started_at = Timestamp::parse("2026-07-20T00:00:01Z")?;
        let finished_at = Timestamp::parse("2026-07-20T00:00:02Z")?;
        let result = CommandResult::accepted(CommandId::new(), accepted_at)
            .start(started_at)?
            .succeed(
                EffectStage::PostconditionMet,
                CommandOutcome::Acknowledged,
                finished_at,
            )?;
        assert_eq!(result.lifecycle(), CommandLifecycle::Succeeded);
        assert!(result.outcome().is_some());
        assert!(result.error().is_none());
        Ok(())
    }

    #[test]
    fn terminal_state_cannot_restart() -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let result = CommandResult::accepted(CommandId::new(), at.clone())
            .start(at.clone())?
            .succeed(
                EffectStage::PostconditionMet,
                CommandOutcome::Acknowledged,
                at.clone(),
            )?;
        assert_eq!(
            result.start(at),
            Err(ResultInvariantError::IllegalTransition)
        );
        Ok(())
    }

    #[test]
    fn additive_response_fields_are_tolerated() -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let result = CommandResult::accepted(CommandId::new(), at);
        let mut encoded = serde_json::to_value(result)?;
        let object = encoded
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("command result must encode as an object"))?;
        object.insert("future_additive_field".to_owned(), serde_json::json!(true));
        let decoded: CommandResult = serde_json::from_value(encoded)?;
        assert_eq!(decoded.lifecycle(), CommandLifecycle::Accepted);
        Ok(())
    }

    #[test]
    fn deserialized_warning_bounds_are_revalidated() -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let result = CommandResult::accepted(CommandId::new(), at);
        let mut encoded = serde_json::to_value(result)?;
        let object = encoded
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("command result must encode as an object"))?;
        object.insert(
            "warnings".to_owned(),
            serde_json::json!([{"code": "", "message": "not valid"}]),
        );
        let decoded: CommandResult = serde_json::from_value(encoded)?;
        assert_eq!(
            decoded.validate(),
            Err(ResultInvariantError::InvalidWarning(
                WarningValidationError::Code
            ))
        );
        Ok(())
    }

    #[test]
    fn queued_commands_may_cancel_or_deadline_before_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let accepted = CommandResult::accepted(CommandId::new(), at.clone());
        assert_eq!(
            accepted.clone().fail(
                CommandLifecycle::Failed,
                problem(ErrorCode::Internal, EffectStage::None)?,
                at.clone(),
            ),
            Err(ResultInvariantError::IllegalTransition)
        );
        assert_eq!(
            accepted.clone().fail(
                CommandLifecycle::CancelledAfterEffect,
                problem(ErrorCode::Internal, EffectStage::ButtonPressed)?,
                at.clone(),
            ),
            Err(ResultInvariantError::IllegalTransition)
        );
        for (lifecycle, code) in [
            (
                CommandLifecycle::CancelledBeforeEffect,
                ErrorCode::CancelledBeforeEffect,
            ),
            (
                CommandLifecycle::DeadlineBeforeEffect,
                ErrorCode::DeadlineExceededBeforeEffect,
            ),
        ] {
            let terminal =
                accepted
                    .clone()
                    .fail(lifecycle, problem(code, EffectStage::None)?, at.clone())?;
            assert_eq!(terminal.lifecycle(), lifecycle);
            assert!(terminal.started_at.is_none());

            let decoded: CommandResult = serde_json::from_value(serde_json::to_value(terminal)?)?;
            assert_eq!(decoded.validate(), Ok(()));
            assert!(decoded.started_at.is_none());
        }
        Ok(())
    }

    #[test]
    fn running_commands_may_cancel_or_deadline_before_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let accepted_at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let started_at = Timestamp::parse("2026-07-20T00:00:01Z")?;
        let finished_at = Timestamp::parse("2026-07-20T00:00:02Z")?;
        for (lifecycle, code) in [
            (
                CommandLifecycle::CancelledBeforeEffect,
                ErrorCode::CancelledBeforeEffect,
            ),
            (
                CommandLifecycle::DeadlineBeforeEffect,
                ErrorCode::DeadlineExceededBeforeEffect,
            ),
        ] {
            let terminal = CommandResult::accepted(CommandId::new(), accepted_at.clone())
                .start(started_at.clone())?
                .fail(
                    lifecycle,
                    problem(code, EffectStage::None)?,
                    finished_at.clone(),
                )?;
            assert_eq!(terminal.lifecycle(), lifecycle);
            assert!(terminal.started_at.is_some());

            let decoded: CommandResult = serde_json::from_value(serde_json::to_value(terminal)?)?;
            assert_eq!(decoded.validate(), Ok(()));
            assert!(decoded.started_at.is_some());
        }
        Ok(())
    }

    #[test]
    fn lifecycle_enforces_effect_truth() -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let running = CommandResult::accepted(CommandId::new(), at.clone()).start(at.clone())?;
        let read_only_success = running.clone().succeed(
            EffectStage::None,
            CommandOutcome::Probe { ready: true },
            at.clone(),
        )?;
        assert_eq!(read_only_success.effect_stage(), EffectStage::None);
        assert_eq!(
            running.fail(
                CommandLifecycle::DeadlineAfterEffect,
                problem(ErrorCode::DeadlineExceededAfterEffect, EffectStage::None)?,
                at,
            ),
            Err(ResultInvariantError::EffectDoesNotMatchLifecycle)
        );
        Ok(())
    }

    #[test]
    fn specialized_error_codes_match_terminal_lifecycle() -> Result<(), Box<dyn std::error::Error>>
    {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let running = CommandResult::accepted(CommandId::new(), at.clone()).start(at.clone())?;
        assert_eq!(
            running.clone().fail(
                CommandLifecycle::DeadlineAfterEffect,
                problem(ErrorCode::Internal, EffectStage::ButtonPressed)?,
                at.clone(),
            ),
            Err(ResultInvariantError::ProblemCodeMismatch)
        );
        assert_eq!(
            running.fail(
                CommandLifecycle::Failed,
                problem(ErrorCode::CancelledAfterEffect, EffectStage::ButtonPressed)?,
                at,
            ),
            Err(ResultInvariantError::ProblemCodeMismatch)
        );
        Ok(())
    }

    #[test]
    fn deadline_lifecycle_keeps_short_wire_names() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_string(&CommandLifecycle::DeadlineBeforeEffect)?,
            r#""deadline_before_effect""#
        );
        assert_eq!(
            serde_json::to_string(&CommandLifecycle::DeadlineAfterEffect)?,
            r#""deadline_after_effect""#
        );
        Ok(())
    }

    #[test]
    fn timestamps_are_monotonic() -> Result<(), Box<dyn std::error::Error>> {
        let accepted = Timestamp::parse("2026-07-20T00:00:02Z")?;
        let earlier = Timestamp::parse("2026-07-20T00:00:01Z")?;
        assert_eq!(
            CommandResult::accepted(CommandId::new(), accepted).start(earlier),
            Err(ResultInvariantError::TimestampOrder)
        );
        Ok(())
    }

    #[test]
    fn nested_problem_and_effect_are_revalidated() -> Result<(), Box<dyn std::error::Error>> {
        let at = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let failed = CommandResult::accepted(CommandId::new(), at.clone())
            .start(at.clone())?
            .fail(
                CommandLifecycle::Failed,
                problem(ErrorCode::Internal, EffectStage::ButtonPressed)?,
                at,
            )?;

        let mut effect_mismatch = serde_json::to_value(&failed)?;
        effect_mismatch["effect_stage"] = serde_json::json!("pointer_moved");
        let decoded: CommandResult = serde_json::from_value(effect_mismatch)?;
        assert_eq!(
            decoded.validate(),
            Err(ResultInvariantError::ProblemEffectMismatch)
        );

        let mut invalid_problem = serde_json::to_value(&failed)?;
        invalid_problem["error"]["detail"] = serde_json::json!("");
        let decoded: CommandResult = serde_json::from_value(invalid_problem)?;
        assert_eq!(
            decoded.validate(),
            Err(ResultInvariantError::InvalidProblem(
                ProblemValidationError::InvalidTextLength
            ))
        );
        Ok(())
    }
}
