//! Strict command envelope and version-one command shapes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ApplicationLaunchCommand, CommandId, ControlLeaseId, DesktopGeneration, DesktopId, Point,
    ProcessStatusCommand, ProcessTerminateCommand, ProtocolVersion, RequestId, Timestamp,
};

/// Maximum delay accepted by one XTEST-timed primitive.
pub const MAX_XTEST_DELAY_MS: u32 = 10_000;

/// Maximum duration carried by a single pointer motion primitive.
pub const MAX_POINTER_MOVE_DURATION_MS: u32 = MAX_XTEST_DELAY_MS;

/// Smallest structurally valid core X11 physical keycode.
pub const MIN_PHYSICAL_KEYCODE: u8 = 8;

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
        if self.curve == PointerCurve::Instant
            && self.duration_ms.is_some_and(|duration| duration != 0)
        {
            return Err(EnvelopeValidationError::InstantPointerDuration);
        }
        Ok(())
    }
}

/// A raw X11 physical pointer-button transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerButtonCommand {
    /// Non-zero X11 physical button detail.
    #[schemars(range(min = 1))]
    pub button: u8,
    /// Diagnostic opt-in for duplicate down or unowned up.
    pub allow_redundant: bool,
}

impl PointerButtonCommand {
    /// Rejects X11's reserved zero button detail.
    pub fn validate(self) -> Result<(), EnvelopeValidationError> {
        if self.button == 0 {
            return Err(EnvelopeValidationError::InvalidPhysicalButton);
        }
        Ok(())
    }
}

/// A raw core-X11 physical key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardKeyCommand {
    /// Core keycode resolved by an advanced client for this desktop keymap.
    #[schemars(range(min = MIN_PHYSICAL_KEYCODE))]
    pub keycode: u8,
    /// Diagnostic opt-in for duplicate down or unowned up.
    pub allow_redundant: bool,
}

impl KeyboardKeyCommand {
    /// Rejects reserved core-X11 keycodes below eight.
    pub fn validate(self) -> Result<(), EnvelopeValidationError> {
        if self.keycode < MIN_PHYSICAL_KEYCODE {
            return Err(EnvelopeValidationError::InvalidPhysicalKeycode);
        }
        Ok(())
    }
}

/// Empty request to conservatively release only Xenoteer-owned input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputResetCommand {}

/// A version-one command body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Probe command used by the Phase-0 composition skeleton.
    DesktopProbe(DesktopProbeCommand),
    /// Move the global physical pointer.
    PointerMove(PointerMoveCommand),
    /// Press one raw physical pointer button.
    PointerButtonDown(PointerButtonCommand),
    /// Release one raw physical pointer button.
    PointerButtonUp(PointerButtonCommand),
    /// Press one raw physical core-X11 keycode.
    KeyboardKeyDown(KeyboardKeyCommand),
    /// Release one raw physical core-X11 keycode.
    KeyboardKeyUp(KeyboardKeyCommand),
    /// Conservatively release only input owned by Xenoteer.
    InputReset(InputResetCommand),
    /// Launch a configured application profile without a shell.
    ApplicationLaunch(ApplicationLaunchCommand),
    /// Terminate a verified managed process group.
    ProcessTerminate(ProcessTerminateCommand),
    /// Read current status for an exact managed process reference.
    ProcessStatus(ProcessStatusCommand),
}

impl Command {
    /// Validates protocol-level shape and limits.
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        match self {
            Self::DesktopProbe(_) => Ok(()),
            Self::PointerMove(command) => command.validate(),
            Self::PointerButtonDown(command) | Self::PointerButtonUp(command) => command.validate(),
            Self::KeyboardKeyDown(command) | Self::KeyboardKeyUp(command) => command.validate(),
            Self::InputReset(_) => Ok(()),
            Self::ApplicationLaunch(command) => command.validate().map_err(Into::into),
            Self::ProcessTerminate(command) => command.validate().map_err(Into::into),
            Self::ProcessStatus(command) => command.process.validate().map_err(Into::into),
        }
    }

    /// Returns whether command admission requires the exclusive controller lease.
    #[must_use]
    pub const fn requires_control_lease(&self) -> bool {
        matches!(
            self,
            Self::PointerMove(_)
                | Self::PointerButtonDown(_)
                | Self::PointerButtonUp(_)
                | Self::KeyboardKeyDown(_)
                | Self::KeyboardKeyUp(_)
                | Self::InputReset(_)
        )
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

    /// Creates a checked physical-input envelope with its controller lease.
    pub fn new_with_lease(
        protocol_version: ProtocolVersion,
        request_id: RequestId,
        command_id: CommandId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        lease_id: ControlLeaseId,
        command: Command,
    ) -> Result<Self, EnvelopeValidationError> {
        let envelope = Self {
            protocol_version,
            request_id,
            command_id,
            desktop_id,
            desktop_generation,
            lease_id: Some(lease_id),
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
        if self.command.requires_control_lease() && self.lease_id.is_none() {
            return Err(EnvelopeValidationError::LeaseRequired);
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
    /// Instant motion accepts only an omitted or zero duration.
    #[error("instant pointer motion requires an omitted or zero duration")]
    InstantPointerDuration,
    /// Public envelope identifiers must never use UUID nil.
    #[error("command envelope contains a nil identifier")]
    NilIdentifier,
    /// Caller-generated command identifiers use UUID version 4 or 7.
    #[error("command identifier must be UUID version 4 or 7")]
    CommandIdVersion,
    /// A physical-input command omitted its controller lease capability.
    #[error("physical-input command requires a controller lease")]
    LeaseRequired,
    /// X11 button detail zero is reserved.
    #[error("physical pointer button must be non-zero")]
    InvalidPhysicalButton,
    /// Core X11 keycodes below eight are reserved.
    #[error("physical keycode is outside the core X11 range")]
    InvalidPhysicalKeycode,
    /// A registered-application/process command failed validation.
    #[error("managed process command is invalid: {0}")]
    Process(#[from] crate::ProcessValidationError),
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

        for duration_ms in [None, Some(0)] {
            let instant = PointerMoveCommand {
                target: Point::new(1, 2),
                duration_ms,
                curve: PointerCurve::Instant,
            };
            assert_eq!(instant.validate(), Ok(()));
        }

        let rejected_instant = PointerMoveCommand {
            target: Point::new(1, 2),
            duration_ms: Some(1),
            curve: PointerCurve::Instant,
        };
        assert_eq!(
            rejected_instant.validate(),
            Err(EnvelopeValidationError::InstantPointerDuration)
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

    #[test]
    fn physical_input_constructor_requires_and_accepts_a_lease() {
        let command = Command::PointerMove(PointerMoveCommand {
            target: Point::new(10, 20),
            duration_ms: Some(50),
            curve: PointerCurve::Smooth,
        });
        assert_eq!(
            CommandEnvelope::new(
                ProtocolVersion::V1_0,
                RequestId::new(),
                CommandId::new(),
                DesktopId::new(),
                DesktopGeneration::new(),
                command.clone(),
            ),
            Err(EnvelopeValidationError::LeaseRequired)
        );
        assert!(
            CommandEnvelope::new_with_lease(
                ProtocolVersion::V1_0,
                RequestId::new(),
                CommandId::new(),
                DesktopId::new(),
                DesktopGeneration::new(),
                ControlLeaseId::new(),
                command,
            )
            .is_ok()
        );
    }
}
