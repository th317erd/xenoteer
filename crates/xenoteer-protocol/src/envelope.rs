//! Command envelope and Phase-0 command shapes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CommandId, ControlLeaseId, DesktopGeneration, DesktopId, Point, ProtocolVersion, RequestId,
    Timestamp,
};

/// Maximum delay accepted by one XTEST-timed primitive.
pub const MAX_XTEST_DELAY_MS: u32 = 10_000;

/// Maximum duration carried by a single pointer motion primitive.
pub const MAX_POINTER_MOVE_DURATION_MS: u32 = MAX_XTEST_DELAY_MS;

/// The amount of trace detail requested by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TracePolicy {
    /// Retain only normal audit metadata.
    Normal,
    /// Retain bounded diagnostic effect evidence.
    Detailed,
}

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

/// A physical pointer movement request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerMoveCommand {
    /// Target root-physical coordinate.
    pub target: Point,
    /// Requested whole-path duration. Omission selects the configured default.
    #[schemars(range(max = MAX_POINTER_MOVE_DURATION_MS))]
    pub duration_ms: Option<u32>,
    /// Interpolation curve.
    pub curve: PointerCurve,
}

impl PointerMoveCommand {
    /// Validates protocol-level duration limits.
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        if self
            .duration_ms
            .is_some_and(|duration| duration > MAX_POINTER_MOVE_DURATION_MS)
        {
            return Err(EnvelopeValidationError::PointerDurationTooLong);
        }
        Ok(())
    }
}

/// A version-one command body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Probe command used by the Phase-0 composition skeleton.
    DesktopProbe(DesktopProbeCommand),
    /// Move the global physical pointer.
    PointerMove(PointerMoveCommand),
}

impl Command {
    /// Validates protocol-level shape and limits.
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        match self {
            Self::DesktopProbe(_) => Ok(()),
            Self::PointerMove(command) => command.validate(),
        }
    }
}

/// Empty, strictly decoded Phase-0 desktop probe parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopProbeCommand {}

/// A complete command submission envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    /// Requested protocol version.
    pub protocol_version: ProtocolVersion,
    /// Transport request correlation identifier.
    pub request_id: RequestId,
    /// Caller-generated deduplication identifier.
    pub command_id: CommandId,
    /// Target desktop.
    pub desktop_id: DesktopId,
    /// Target desktop generation observed by the caller.
    pub desktop_generation: DesktopGeneration,
    /// Physical controller lease when required by the command.
    pub lease_id: Option<ControlLeaseId>,
    /// Absolute UTC deadline.
    pub deadline: Option<Timestamp>,
    /// Requested trace detail.
    pub trace_policy: Option<TracePolicy>,
    /// Typed operation.
    pub command: Command,
}

impl CommandEnvelope {
    /// Creates a checked envelope with optional behavior fields unset.
    pub fn new(
        protocol_version: ProtocolVersion,
        request_id: RequestId,
        command_id: CommandId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        command: Command,
    ) -> Result<Self, EnvelopeValidationError> {
        let envelope = Self {
            protocol_version,
            request_id,
            command_id,
            desktop_id,
            desktop_generation,
            lease_id: None,
            deadline: None,
            trace_policy: None,
            command,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Attaches the physical controller lease required by mutating input.
    #[must_use]
    pub fn with_lease(mut self, lease_id: ControlLeaseId) -> Self {
        self.lease_id = Some(lease_id);
        self
    }

    /// Attaches an absolute UTC deadline.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Timestamp) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Attaches the requested trace detail policy.
    #[must_use]
    pub fn with_trace_policy(mut self, trace_policy: TracePolicy) -> Self {
        self.trace_policy = Some(trace_policy);
        self
    }

    /// Validates protocol-level invariants before domain admission.
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        if self.protocol_version.major() != ProtocolVersion::V1_0.major() {
            return Err(EnvelopeValidationError::UnsupportedMajor);
        }
        if self.request_id.as_uuid().is_nil()
            || self.command_id.as_uuid().is_nil()
            || self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
            || self.lease_id.is_some_and(|lease| lease.as_uuid().is_nil())
        {
            return Err(EnvelopeValidationError::NilIdentifier);
        }
        if !matches!(self.command_id.as_uuid().get_version_num(), 4 | 7) {
            return Err(EnvelopeValidationError::CommandIdVersion);
        }
        self.command.validate()
    }
}

/// Protocol-level command validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnvelopeValidationError {
    /// The server does not implement the requested protocol major.
    #[error("unsupported protocol major")]
    UnsupportedMajor,
    /// A pointer duration exceeds the protocol ceiling.
    #[error("pointer move duration exceeds protocol maximum")]
    PointerDurationTooLong,
    /// Public envelope identifiers must never use UUID nil.
    #[error("command envelope contains a nil identifier")]
    NilIdentifier,
    /// Caller-generated command identifiers use UUID version 4 or 7.
    #[error("command identifier must be UUID version 4 or 7")]
    CommandIdVersion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_command_fields_are_rejected() {
        let json = r#"{"type":"desktop_probe","typo":true}"#;
        assert!(serde_json::from_str::<Command>(json).is_err());
    }

    #[test]
    fn duration_boundary_is_checked() {
        let accepted = PointerMoveCommand {
            target: Point::new(1, 2),
            duration_ms: Some(MAX_POINTER_MOVE_DURATION_MS),
            curve: PointerCurve::Smooth,
        };
        assert_eq!(accepted.validate(), Ok(()));

        let rejected = PointerMoveCommand {
            target: Point::new(1, 2),
            duration_ms: Some(MAX_POINTER_MOVE_DURATION_MS + 1),
            curve: PointerCurve::Smooth,
        };
        assert_eq!(
            rejected.validate(),
            Err(EnvelopeValidationError::PointerDurationTooLong)
        );
    }

    #[test]
    fn envelope_rejects_nil_identifiers() {
        let envelope = CommandEnvelope {
            protocol_version: ProtocolVersion::V1_0,
            request_id: RequestId::new(),
            command_id: CommandId::from_uuid(uuid::Uuid::nil()),
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            lease_id: None,
            deadline: None,
            trace_policy: None,
            command: Command::DesktopProbe(DesktopProbeCommand {}),
        };
        assert_eq!(
            envelope.validate(),
            Err(EnvelopeValidationError::NilIdentifier)
        );
    }
}
