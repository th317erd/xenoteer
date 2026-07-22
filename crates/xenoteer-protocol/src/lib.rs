//! Versioned, backend-independent wire types for Xenoteer.
//!
//! This crate is intentionally portable and contains no X11, D-Bus, Axum, or
//! runtime types. Deserialized values should be validated at admission with the
//! checked constructors and `validate` methods exposed by each module.

#![forbid(unsafe_code)]

pub mod capabilities;
pub mod envelope;
pub mod geometry;
pub mod ids;
pub mod lease;
pub mod problem;
pub mod process;
pub mod result;
pub mod schema;
pub mod timestamp;
pub mod version;
pub mod websocket;

pub use capabilities::{
    Capability, CapabilityId, CapabilityIdError, CapabilityReport, CapabilityReportError,
    CapabilityStatus, CapabilityValidationError, MAX_CAPABILITIES,
};
pub use envelope::{
    Command, CommandEnvelope, DesktopProbeCommand, EnvelopeValidationError, InputResetCommand,
    KeyboardKeyCommand, MAX_POINTER_MOVE_DURATION_MS, MAX_XTEST_DELAY_MS, MIN_PHYSICAL_KEYCODE,
    PointerButtonCommand, PointerCurve, PointerMoveCommand, TracePolicy,
};
pub use geometry::{CoordinateSpace, GeometryError, Point, Rect, Size};
pub use ids::{
    ArtifactId, CommandId, ConnectionId, ControlLeaseId, DesktopGeneration, DesktopId, LaunchId,
    RequestId,
};
pub use lease::{
    LeaseAcquireRequest, LeaseAvailability, LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView,
    LeaseValidationError, MAX_LEASE_TTL_MS,
};
pub use problem::{
    ErrorCode, MAX_PROBLEM_DETAIL_BYTES, MAX_PROBLEM_DETAIL_KEY_BYTES, MAX_PROBLEM_DETAILS,
    MAX_PROBLEM_DETAILS_ENCODED_BYTES, MAX_PROBLEM_INSTANCE_BYTES, MAX_PROBLEM_TITLE_BYTES,
    MAX_PROBLEM_TYPE_BYTES, Problem, ProblemValidationError, RetryAdvice,
};
pub use process::{
    ApplicationArgument, ApplicationId, ApplicationLaunchCommand, MAX_APPLICATION_ARGUMENT_BYTES,
    MAX_APPLICATION_ARGUMENTS, MAX_APPLICATION_ID_BYTES, MAX_TERMINATION_GRACE_MS, ProcessExit,
    ProcessExitedEvent, ProcessRef, ProcessState, ProcessStatusCommand, ProcessTerminateCommand,
    ProcessValidationError, ProcessView,
};
pub use result::{
    CommandLifecycle, CommandOutcome, CommandResult, EffectStage, ResultInvariantError, Warning,
    WarningValidationError,
};
pub use timestamp::{Timestamp, TimestampError};
pub use version::{ProtocolVersion, VersionRange};
pub use websocket::{
    ACTION_LIFECYCLE_TOPIC, COMMAND_LIFECYCLE_TOPIC, ClientHello, EventResumeRequest,
    EventResumeStatus, EventResyncReason, EventTopic, MAX_EVENT_PAYLOAD_BYTES,
    MAX_EVENT_TOPIC_BYTES, MAX_EVENT_TOPICS, NormalizedEvent, PROCESS_EXITED_TOPIC, SequencedEvent,
    WebSocketClientDescriptor, WebSocketClientMessage, WebSocketServerMessage,
    WebSocketValidationError, WelcomeDesktop, WelcomeDesktopState, WelcomeLimits, WelcomePrincipal,
    WelcomeResume,
};
