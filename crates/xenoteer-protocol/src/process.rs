//! Registered-application commands and PID-reuse-safe process references.

use core::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{DesktopGeneration, LaunchId};

/// Maximum UTF-8 length of a registered application identifier.
pub const MAX_APPLICATION_ID_BYTES: usize = 128;
/// Maximum UTF-8 length of one profile-approved application argument.
pub const MAX_APPLICATION_ARGUMENT_BYTES: usize = 1_024;
/// Maximum caller arguments accepted by one registered launch.
pub const MAX_APPLICATION_ARGUMENTS: usize = 64;
/// Protocol ceiling for graceful process termination.
pub const MAX_TERMINATION_GRACE_MS: u32 = 30_000;

/// A stable identifier for a configured application profile.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "application_id_schema")]
pub struct ApplicationId(String);

fn application_id_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_APPLICATION_ID_BYTES,
        "pattern": "^[a-z0-9][a-z0-9._-]*$"
    })
}

impl ApplicationId {
    /// Creates a checked registered-profile identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, ProcessValidationError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_APPLICATION_ID_BYTES {
            return Err(ProcessValidationError::ApplicationId);
        }
        if !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        }) {
            return Err(ProcessValidationError::ApplicationId);
        }
        Ok(Self(value))
    }

    /// Returns the stable profile identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApplicationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ApplicationId")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ApplicationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ApplicationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A bounded, non-control application argument.
#[derive(Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(schema_with = "application_argument_schema")]
pub struct ApplicationArgument(String);

fn application_argument_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "maxLength": MAX_APPLICATION_ARGUMENT_BYTES
    })
}

impl ApplicationArgument {
    /// Creates a checked argument. Empty arguments are valid argv entries.
    pub fn new(value: impl Into<String>) -> Result<Self, ProcessValidationError> {
        let value = value.into();
        if value.len() > MAX_APPLICATION_ARGUMENT_BYTES || value.chars().any(char::is_control) {
            return Err(ProcessValidationError::ApplicationArgument);
        }
        Ok(Self(value))
    }

    /// Returns the argument exactly as approved for argv construction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApplicationArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ApplicationArgument")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ApplicationArgument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ApplicationArgument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A reference safe against desktop restart and Linux PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessRef {
    /// Desktop lifetime that owns the child.
    pub desktop_generation: DesktopGeneration,
    /// Positive Linux process identifier.
    #[schemars(range(min = 1))]
    pub pid: u32,
    /// Field 22 from `/proc/{pid}/stat`, measured in boot clock ticks.
    #[schemars(range(min = 1))]
    pub proc_start_ticks: u64,
    /// Server-generated managed launch identity.
    pub launch_id: LaunchId,
}

/// Request-direction representation of [`ProcessRef`].
///
/// The public reference is also emitted in responses and therefore accepts
/// additive output fields. Every request occurrence deserializes through this
/// closed shape so a misspelled identity claim is never ignored.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "ProcessRef")]
pub(crate) struct StrictProcessRef {
    desktop_generation: DesktopGeneration,
    #[schemars(range(min = 1))]
    pid: u32,
    #[schemars(range(min = 1))]
    proc_start_ticks: u64,
    launch_id: LaunchId,
}

impl From<StrictProcessRef> for ProcessRef {
    fn from(value: StrictProcessRef) -> Self {
        Self {
            desktop_generation: value.desktop_generation,
            pid: value.pid,
            proc_start_ticks: value.proc_start_ticks,
            launch_id: value.launch_id,
        }
    }
}

pub(crate) fn deserialize_strict_process_ref<'de, D>(
    deserializer: D,
) -> Result<ProcessRef, D::Error>
where
    D: Deserializer<'de>,
{
    StrictProcessRef::deserialize(deserializer).map(Into::into)
}

impl ProcessRef {
    /// Validates generation, PID, start time, and launch identity.
    pub fn validate(self) -> Result<(), ProcessValidationError> {
        if self.desktop_generation.as_uuid().is_nil() || self.launch_id.as_uuid().is_nil() {
            return Err(ProcessValidationError::NilIdentifier);
        }
        if self.pid == 0 || self.proc_start_ticks == 0 {
            return Err(ProcessValidationError::ProcessReference);
        }
        Ok(())
    }
}

/// Launches only a named registered profile with bounded argv additions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationLaunchCommand {
    /// Registered profile selected by stable ID.
    pub application: ApplicationId,
    /// Profile-approved argv additions; never a shell command string.
    #[schemars(length(max = MAX_APPLICATION_ARGUMENTS))]
    pub arguments: Vec<ApplicationArgument>,
}

impl ApplicationLaunchCommand {
    /// Validates the aggregate argument count.
    pub fn validate(&self) -> Result<(), ProcessValidationError> {
        if self.arguments.len() > MAX_APPLICATION_ARGUMENTS {
            return Err(ProcessValidationError::TooManyArguments);
        }
        Ok(())
    }
}

/// Requests managed group termination after reference revalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessTerminateCommand {
    /// Exact managed process identity.
    #[serde(deserialize_with = "deserialize_strict_process_ref")]
    #[schemars(with = "StrictProcessRef")]
    pub process: ProcessRef,
    /// Optional SIGTERM grace before SIGKILL; omission selects profile policy.
    #[schemars(range(max = MAX_TERMINATION_GRACE_MS))]
    pub grace_ms: Option<u32>,
}

impl ProcessTerminateCommand {
    /// Validates the reference and grace ceiling.
    pub fn validate(self) -> Result<(), ProcessValidationError> {
        self.process.validate()?;
        if self
            .grace_ms
            .is_some_and(|grace| grace > MAX_TERMINATION_GRACE_MS)
        {
            return Err(ProcessValidationError::TerminationGrace);
        }
        Ok(())
    }
}

/// Requests current managed status for an exact reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessStatusCommand {
    /// Exact managed process identity.
    #[serde(deserialize_with = "deserialize_strict_process_ref")]
    #[schemars(with = "StrictProcessRef")]
    pub process: ProcessRef,
}

/// Coarse managed child lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// Spawn is admitted but exec identity is not yet published.
    Starting,
    /// The verified child/process group is running.
    Running,
    /// SIGTERM/SIGKILL termination is in progress.
    Terminating,
    /// The child was awaited and reaped.
    Exited,
}

/// Reaped Unix child status without unbounded output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessExit {
    /// Normal exit status, mutually exclusive with `signal`.
    pub code: Option<i32>,
    /// Terminating signal, mutually exclusive with `code`.
    #[schemars(range(min = 1, max = 255))]
    pub signal: Option<u8>,
    /// Whether the OS reported a core dump.
    pub core_dumped: bool,
}

impl ProcessExit {
    /// Requires exactly one normal-code or signal outcome.
    pub fn validate(self) -> Result<(), ProcessValidationError> {
        if self.code.is_some() == self.signal.is_some() {
            return Err(ProcessValidationError::ExitShape);
        }
        if self.signal == Some(0) {
            return Err(ProcessValidationError::ExitShape);
        }
        Ok(())
    }
}

/// Current status of a managed application child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessView {
    /// Exact process reference.
    pub process: ProcessRef,
    /// Current managed lifecycle.
    pub state: ProcessState,
    /// Exit details present exactly when `state` is `exited`.
    pub exit: Option<ProcessExit>,
}

impl ProcessView {
    /// Validates the reference and lifecycle-dependent exit payload.
    pub fn validate(&self) -> Result<(), ProcessValidationError> {
        self.process.validate()?;
        if (self.state == ProcessState::Exited) != self.exit.is_some() {
            return Err(ProcessValidationError::ProcessView);
        }
        if let Some(exit) = self.exit {
            exit.validate()?;
        }
        Ok(())
    }
}

/// Public, output-free terminal lifecycle event for one registered application.
///
/// The authenticated owner is deliberately absent from this wire shape. Event
/// routing applies that audience before serialization, and neither captured
/// stdout/stderr nor the identity of a termination requester is disclosed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessExitedEvent {
    /// Stable image-owned application profile that produced the process.
    pub application: ApplicationId,
    /// Exact reaped process identity and terminal status.
    pub process: ProcessView,
    /// Whether managed TERM/KILL cleanup, rather than natural exit, was requested.
    pub termination_requested: bool,
    /// Whether graceful termination required SIGKILL escalation.
    pub forced_escalation: bool,
}

impl ProcessExitedEvent {
    /// Requires a terminal, internally consistent process view.
    pub fn validate(&self) -> Result<(), ProcessValidationError> {
        self.process.validate()?;
        if self.process.state != ProcessState::Exited {
            return Err(ProcessValidationError::ProcessEvent);
        }
        if self.forced_escalation && !self.termination_requested {
            return Err(ProcessValidationError::ProcessEvent);
        }
        Ok(())
    }
}

/// A process wire-shape validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProcessValidationError {
    /// A public UUID is nil.
    #[error("process message contains a nil identifier")]
    NilIdentifier,
    /// PID or `/proc` start ticks are zero.
    #[error("process reference requires positive PID and start ticks")]
    ProcessReference,
    /// The registered application identifier is malformed.
    #[error("registered application identifier is invalid")]
    ApplicationId,
    /// One argv value is oversized or contains control characters.
    #[error("application argument is invalid")]
    ApplicationArgument,
    /// The launch contains too many argv additions.
    #[error("application argument count exceeds protocol maximum")]
    TooManyArguments,
    /// Termination grace exceeds the protocol ceiling.
    #[error("termination grace exceeds protocol maximum")]
    TerminationGrace,
    /// Exit status must contain exactly one code or signal.
    #[error("process exit shape is invalid")]
    ExitShape,
    /// Process state and exit details disagree.
    #[error("process view state and exit details disagree")]
    ProcessView,
    /// A process lifecycle event is not terminal or has inconsistent cleanup flags.
    #[error("process lifecycle event is inconsistent")]
    ProcessEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_identifiers_are_checked_during_decode() {
        assert!(serde_json::from_str::<ApplicationId>(r#""recorder.x11""#).is_ok());
        assert!(serde_json::from_str::<ApplicationId>(r#""../../bin/sh""#).is_err());
        assert!(serde_json::from_str::<ApplicationArgument>(r#""line\nfeed""#).is_err());
    }

    #[test]
    fn process_reference_rejects_pid_reuse_ambiguity() {
        let reference = ProcessRef {
            desktop_generation: DesktopGeneration::new(),
            pid: 42,
            proc_start_ticks: 0,
            launch_id: LaunchId::new(),
        };
        assert_eq!(
            reference.validate(),
            Err(ProcessValidationError::ProcessReference)
        );
    }

    #[test]
    fn process_exit_events_are_terminal_and_cleanup_flags_are_consistent()
    -> Result<(), Box<dyn std::error::Error>> {
        let process = ProcessRef {
            desktop_generation: DesktopGeneration::new(),
            pid: 42,
            proc_start_ticks: 7,
            launch_id: LaunchId::new(),
        };
        let terminal = ProcessExitedEvent {
            application: ApplicationId::new("fixture")?,
            process: ProcessView {
                process,
                state: ProcessState::Exited,
                exit: Some(ProcessExit {
                    code: Some(0),
                    signal: None,
                    core_dumped: false,
                }),
            },
            termination_requested: false,
            forced_escalation: false,
        };
        terminal.validate()?;

        let mut inconsistent = terminal.clone();
        inconsistent.forced_escalation = true;
        assert_eq!(
            inconsistent.validate(),
            Err(ProcessValidationError::ProcessEvent)
        );

        let mut running = terminal;
        running.process.state = ProcessState::Running;
        running.process.exit = None;
        assert_eq!(
            running.validate(),
            Err(ProcessValidationError::ProcessEvent)
        );
        Ok(())
    }
}
