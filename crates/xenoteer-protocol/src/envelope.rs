//! Strict command envelope and version-one command shapes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::{StrictPoint, deserialize_strict_point};
use crate::version::{StrictProtocolVersion, deserialize_strict_protocol_version};
use crate::{
    ApplicationLaunchCommand, CommandId, ControlLeaseId, DesktopGeneration, DesktopId,
    ElementFocusCommand, ElementInsertTextCommand, ElementInvokeCommand,
    ElementPhysicalClickCommand, ElementScrollCommand, ElementSelectionCommand,
    ElementSetTextCommand, ElementSetValueCommand, InputValidationError, KeyboardChordCommand,
    KeyboardPressCommand, KeyboardSequenceCommand, Point, PointerClickCommand, PointerCurve,
    PointerDragCommand, PointerMoveRelativeCommand, PointerScrollCommand, ProcessStatusCommand,
    ProcessTerminateCommand, ProtocolVersion, RequestId, SelectionClearCommand,
    SelectionSetCommand, TextInsertCommand, Timestamp, WindowActivateCommand, WindowCloseCommand,
    WindowMinimizeCommand, WindowMoveResizeCommand, WindowMoveToWorkspaceCommand,
    WindowSetStateCommand, WindowStackCommand,
};

/// Maximum delay accepted by one XTEST-timed primitive.
pub const MAX_XTEST_DELAY_MS: u32 = 10_000;

/// Maximum duration carried by a single pointer motion primitive.
pub use crate::input::MAX_POINTER_MOVE_DURATION_MS;

/// Smallest structurally valid core X11 physical keycode.
pub const MIN_PHYSICAL_KEYCODE: u8 = 8;

/// The amount of trace detail requested by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TracePolicy {
    /// Do not retain structured trace evidence.
    None,
    /// Retain only normal audit metadata.
    Normal,
    /// Retain bounded diagnostic effect evidence.
    Detailed,
}

/// A physical pointer movement request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointerMoveCommand {
    /// Target root-physical coordinate.
    #[serde(deserialize_with = "deserialize_strict_point")]
    #[schemars(with = "StrictPoint")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Probe command used by the Phase-0 composition skeleton.
    DesktopProbe(DesktopProbeCommand),
    /// Move the global physical pointer.
    PointerMove(PointerMoveCommand),
    /// Move the global pointer relative to its execution-time position.
    PointerMoveRelative(PointerMoveRelativeCommand),
    /// Execute a complete FIFO-atomic click sequence.
    PointerClick(PointerClickCommand),
    /// Execute a complete FIFO-atomic press/move/release drag.
    PointerDrag(PointerDragCommand),
    /// Execute bounded discrete scroll notches.
    PointerScroll(PointerScrollCommand),
    /// Press one raw physical pointer button.
    PointerButtonDown(PointerButtonCommand),
    /// Release one raw physical pointer button.
    PointerButtonUp(PointerButtonCommand),
    /// Press one raw physical core-X11 keycode.
    KeyboardKeyDown(KeyboardKeyCommand),
    /// Release one raw physical core-X11 keycode.
    KeyboardKeyUp(KeyboardKeyCommand),
    /// Press and release one named, scalar, or raw key.
    KeyboardPress(KeyboardPressCommand),
    /// Press a modifier-first chord and release it in reverse order.
    KeyboardChord(KeyboardChordCommand),
    /// Execute complete keyboard units without FIFO interleaving.
    KeyboardSequence(KeyboardSequenceCommand),
    /// Conservatively release only input owned by Xenoteer.
    InputReset(InputResetCommand),
    /// Launch a configured application profile without a shell.
    ApplicationLaunch(ApplicationLaunchCommand),
    /// Terminate a verified managed process group.
    ProcessTerminate(ProcessTerminateCommand),
    /// Read current status for an exact managed process reference.
    ProcessStatus(ProcessStatusCommand),
    /// Ask the window manager to activate one exact window birth.
    WindowActivate(WindowActivateCommand),
    /// Ask the window manager or ICCCM client to close one exact window birth.
    WindowClose(WindowCloseCommand),
    /// Add or remove one idempotent window-manager state.
    WindowSetState(WindowSetStateCommand),
    /// Request a desired minimized state.
    WindowMinimize(WindowMinimizeCommand),
    /// Request bounded programmatic window geometry.
    WindowMoveResize(WindowMoveResizeCommand),
    /// Move one exact window birth to a zero-based workspace.
    WindowMoveToWorkspace(WindowMoveToWorkspaceCommand),
    /// Request a best-effort stacking relationship.
    WindowStack(WindowStackCommand),
    /// Acquire and serve one X11 selection value.
    SelectionSet(SelectionSetCommand),
    /// Relinquish Xenoteer's ownership of one X11 selection.
    SelectionClear(SelectionClearCommand),
    /// Insert exact UTF-8 through one bounded strategy.
    TextInsert(TextInsertCommand),
    /// Invoke one semantic AT-SPI action without synthesizing input.
    ElementInvoke(ElementInvokeCommand),
    /// Request semantic focus through AT-SPI Component.
    ElementFocus(ElementFocusCommand),
    /// Set a semantic Value and verify readback.
    ElementSetValue(ElementSetValueCommand),
    /// Mutate an AT-SPI Selection container.
    ElementSelection(ElementSelectionCommand),
    /// Replace editable text semantically.
    ElementSetText(ElementSetTextCommand),
    /// Insert editable text at an explicit character offset.
    ElementInsertText(ElementInsertTextCommand),
    /// Scroll an accessible component semantically.
    ElementScroll(ElementScrollCommand),
    /// Resolve current element geometry and execute serialized physical input.
    ElementPhysicalClick(ElementPhysicalClickCommand),
}

impl Command {
    /// Validates protocol-level shape and limits.
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        match self {
            Self::DesktopProbe(_) => Ok(()),
            Self::PointerMove(command) => command.validate(),
            Self::PointerMoveRelative(command) => command.validate().map_err(Into::into),
            Self::PointerClick(command) => command.validate().map_err(Into::into),
            Self::PointerDrag(command) => command.validate().map_err(Into::into),
            Self::PointerScroll(command) => command.validate().map_err(Into::into),
            Self::PointerButtonDown(command) | Self::PointerButtonUp(command) => command.validate(),
            Self::KeyboardKeyDown(command) | Self::KeyboardKeyUp(command) => command.validate(),
            Self::KeyboardPress(command) => command.validate().map_err(Into::into),
            Self::KeyboardChord(command) => command.validate().map_err(Into::into),
            Self::KeyboardSequence(command) => command.validate().map_err(Into::into),
            Self::InputReset(_) => Ok(()),
            Self::ApplicationLaunch(command) => command.validate().map_err(Into::into),
            Self::ProcessTerminate(command) => command.validate().map_err(Into::into),
            Self::ProcessStatus(command) => command.process.validate().map_err(Into::into),
            Self::WindowActivate(command) => command.validate().map_err(Into::into),
            Self::WindowClose(command) => command.validate().map_err(Into::into),
            Self::WindowSetState(command) => command.validate().map_err(Into::into),
            Self::WindowMinimize(command) => command.validate().map_err(Into::into),
            Self::WindowMoveResize(command) => command.validate().map_err(Into::into),
            Self::WindowMoveToWorkspace(command) => command.validate().map_err(Into::into),
            Self::WindowStack(command) => command.validate().map_err(Into::into),
            Self::SelectionSet(command) => command.validate().map_err(Into::into),
            Self::SelectionClear(_) => Ok(()),
            Self::TextInsert(command) => command.validate().map_err(Into::into),
            Self::ElementInvoke(command) => command.validate().map_err(Into::into),
            Self::ElementFocus(command) => command.validate().map_err(Into::into),
            Self::ElementSetValue(command) => command.validate().map_err(Into::into),
            Self::ElementSelection(command) => command.validate().map_err(Into::into),
            Self::ElementSetText(command) => command.validate().map_err(Into::into),
            Self::ElementInsertText(command) => command.validate().map_err(Into::into),
            Self::ElementScroll(command) => command.validate().map_err(Into::into),
            Self::ElementPhysicalClick(command) => command.validate().map_err(Into::into),
        }
    }

    fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), EnvelopeValidationError> {
        self.validate()?;
        let window = match self {
            Self::WindowActivate(command) => Some(&command.window),
            Self::WindowClose(command) => Some(&command.window),
            Self::WindowSetState(command) => Some(&command.window),
            Self::WindowMinimize(command) => Some(&command.window),
            Self::WindowMoveResize(command) => Some(&command.window),
            Self::WindowMoveToWorkspace(command) => Some(&command.window),
            Self::WindowStack(command) => Some(&command.window),
            Self::PointerClick(command) => command.target.window(),
            _ => None,
        };
        if window.is_some_and(|window| {
            window.desktop_id != desktop_id || window.desktop_generation != desktop_generation
        }) {
            return Err(EnvelopeValidationError::ReferenceScope);
        }
        match self {
            Self::WindowStack(command)
                if command.sibling.as_ref().is_some_and(|sibling| {
                    sibling.desktop_id != desktop_id
                        || sibling.desktop_generation != desktop_generation
                }) =>
            {
                Err(EnvelopeValidationError::ReferenceScope)
            }
            Self::SelectionSet(command) => command
                .validate_for_desktop(desktop_id, desktop_generation)
                .map_err(Into::into),
            Self::TextInsert(command) => command
                .validate_for_desktop(desktop_id, desktop_generation)
                .map_err(Into::into),
            Self::ElementInvoke(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementFocus(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementSetValue(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementSelection(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementSetText(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementInsertText(command) => {
                crate::accessibility_action::validate_command_scope(
                    desktop_id,
                    desktop_generation,
                    &command.element,
                )
                .map_err(Into::into)
            }
            Self::ElementScroll(command) => crate::accessibility_action::validate_command_scope(
                desktop_id,
                desktop_generation,
                &command.element,
            )
            .map_err(Into::into),
            Self::ElementPhysicalClick(command) => {
                crate::accessibility_action::validate_command_scope(
                    desktop_id,
                    desktop_generation,
                    &command.element,
                )
                .map_err(Into::into)
            }
            _ => Ok(()),
        }
    }

    /// Returns whether command admission requires the exclusive controller lease.
    #[must_use]
    pub fn requires_control_lease(&self) -> bool {
        match self {
            Self::TextInsert(command) => command.requires_control_lease(),
            Self::PointerMove(_)
            | Self::PointerMoveRelative(_)
            | Self::PointerClick(_)
            | Self::PointerDrag(_)
            | Self::PointerScroll(_)
            | Self::PointerButtonDown(_)
            | Self::PointerButtonUp(_)
            | Self::KeyboardKeyDown(_)
            | Self::KeyboardKeyUp(_)
            | Self::KeyboardPress(_)
            | Self::KeyboardChord(_)
            | Self::KeyboardSequence(_)
            | Self::InputReset(_)
            | Self::ElementPhysicalClick(_) => true,
            _ => false,
        }
    }
}

/// Empty, strictly decoded Phase-0 desktop probe parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopProbeCommand {}

/// A complete command submission envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    /// Requested protocol version.
    #[serde(deserialize_with = "deserialize_strict_protocol_version")]
    #[schemars(with = "StrictProtocolVersion")]
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
        self.command
            .validate_for_desktop(self.desktop_id, self.desktop_generation)
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
    /// A compound physical-input command failed strict validation.
    #[error("physical input command is invalid: {0}")]
    Input(#[from] InputValidationError),
    /// A registered-application/process command failed validation.
    #[error("managed process command is invalid: {0}")]
    Process(#[from] crate::ProcessValidationError),
    /// A window command failed shape or geometry validation.
    #[error("window command is invalid: {0}")]
    Window(#[from] crate::WindowControlValidationError),
    /// A clipboard or text command failed bounded-content validation.
    #[error("clipboard command is invalid: {0}")]
    Clipboard(#[from] crate::ClipboardValidationError),
    /// A semantic or element-derived physical action failed validation.
    #[error("accessibility action command is invalid: {0}")]
    Accessibility(#[from] crate::AccessibilityActionValidationError),
    /// A nested reference belongs to another desktop lifetime.
    #[error("command reference belongs to another desktop lifetime")]
    ReferenceScope,
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

    #[test]
    fn phase_four_references_are_bound_to_the_envelope_desktop_lifetime()
    -> Result<(), Box<dyn std::error::Error>> {
        let envelope_desktop = DesktopId::new();
        let envelope_generation = DesktopGeneration::new();
        let command = Command::WindowActivate(WindowActivateCommand {
            window: crate::WindowRef {
                desktop_id: DesktopId::new(),
                desktop_generation: envelope_generation,
                xid: 42,
                observed_generation: 1,
                identity_hash: crate::WindowIdentityHash::new("a".repeat(64))?,
            },
            switch_workspace: false,
            fallback: crate::WindowFocusFallback::EwmhOnly,
        });
        assert_eq!(
            CommandEnvelope::new(
                ProtocolVersion::V1_0,
                RequestId::new(),
                CommandId::new(),
                envelope_desktop,
                envelope_generation,
                command,
            ),
            Err(EnvelopeValidationError::ReferenceScope)
        );
        Ok(())
    }

    #[test]
    fn move_to_workspace_is_strict_and_bound_to_the_envelope_lifetime()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command = Command::WindowMoveToWorkspace(WindowMoveToWorkspaceCommand {
            window: crate::WindowRef {
                desktop_id,
                desktop_generation: generation,
                xid: 43,
                observed_generation: 1,
                identity_hash: crate::WindowIdentityHash::new("b".repeat(64))?,
            },
            workspace: 3,
        });
        CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            command,
        )?;

        let unknown = serde_json::json!({
            "type": "window_move_to_workspace",
            "window": {
                "desktop_id": desktop_id,
                "desktop_generation": generation,
                "xid": 43,
                "observed_generation": "1",
                "identity_hash": "b".repeat(64)
            },
            "workspace": 3,
            "sticky": true
        });
        assert!(serde_json::from_value::<Command>(unknown).is_err());
        Ok(())
    }

    #[test]
    fn text_insertion_requires_the_physical_controller_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let command = Command::TextInsert(TextInsertCommand {
            text: crate::TextSource::Inline {
                text: crate::SecretInlineText::new("hello")?,
            },
            target: crate::TextTarget::Window {
                window: crate::WindowRef {
                    desktop_id,
                    desktop_generation,
                    xid: 7,
                    observed_generation: 1,
                    identity_hash: crate::WindowIdentityHash::new("b".repeat(64))?,
                },
            },
            strategy: crate::TextStrategy::Physical,
            clipboard_options: None,
            semantic_options: None,
            auto_policy: None,
        });
        assert_eq!(
            CommandEnvelope::new(
                ProtocolVersion::V1_0,
                RequestId::new(),
                CommandId::new(),
                desktop_id,
                desktop_generation,
                command,
            ),
            Err(EnvelopeValidationError::LeaseRequired)
        );
        Ok(())
    }
}
