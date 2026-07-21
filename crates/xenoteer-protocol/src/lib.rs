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
pub mod problem;
pub mod result;
pub mod schema;
pub mod timestamp;
pub mod version;

pub use capabilities::{
    Capability, CapabilityId, CapabilityReport, CapabilityReportError, CapabilityStatus,
    CapabilityValidationError, MAX_CAPABILITIES,
};
pub use envelope::{
    Command, CommandEnvelope, DesktopProbeCommand, MAX_POINTER_MOVE_DURATION_MS,
    MAX_XTEST_DELAY_MS, PointerCurve, PointerMoveCommand, TracePolicy,
};
pub use geometry::{CoordinateSpace, GeometryError, Point, Rect, Size};
pub use ids::{ArtifactId, CommandId, ControlLeaseId, DesktopGeneration, DesktopId, RequestId};
pub use problem::{
    ErrorCode, MAX_PROBLEM_DETAIL_BYTES, MAX_PROBLEM_DETAIL_KEY_BYTES, MAX_PROBLEM_DETAILS,
    MAX_PROBLEM_DETAILS_ENCODED_BYTES, MAX_PROBLEM_INSTANCE_BYTES, MAX_PROBLEM_TITLE_BYTES,
    MAX_PROBLEM_TYPE_BYTES, Problem, ProblemValidationError, RetryAdvice,
};
pub use result::{
    CommandLifecycle, CommandOutcome, CommandResult, EffectStage, ResultInvariantError, Warning,
    WarningValidationError,
};
pub use timestamp::{Timestamp, TimestampError};
pub use version::{ProtocolVersion, VersionRange};
