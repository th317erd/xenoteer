//! Deterministic coordinator-domain state machines.
//!
//! This module deliberately owns no clocks, random-number generators, hashers,
//! persistence, sockets, or X11 resources. Callers perform those effects and pass
//! their results into these state machines.

mod event_hub;
mod generation;
mod lease;
mod ledger;
mod runtime;
mod time;

use core::fmt;

pub use event_hub::{
    EventHub, EventHubError, EventHubLimits, EventRecord, PublishOutcome, ReplayFailure,
    ReplayResult,
};
pub use generation::{GenerationFence, GenerationFenceError, GenerationToken};
pub use lease::{
    LeaseError, LeaseGrant, LeaseMachine, LeasePhase, LeasePolicy, LeaseSnapshot, RevocationReason,
};
pub use ledger::{
    CanonicalCommandHash, CommandLedger, CommandLedgerError, CommandLedgerLimits, CommandRecord,
    CommandRecordState, IdempotencyDecision,
};
pub use runtime::{
    BoxCoordinatorFuture, CancelCommandOutcome, CommandEffect, CommandEventMapper, CommandExecutor,
    CommandSubmission, CommandTerminal, CoordinatorError, CoordinatorEvent, CoordinatorHandle,
    CoordinatorSettings, EventSubscription, ExecutionContext, ExecutionOutcome, ExecutionStop,
    LeaseRequirement, ResetOutcome, ResetRequest, ResetRetryOutcome, TerminalCause,
    spawn_coordinator, spawn_coordinator_with_event_mapper,
};
pub use time::MonotonicMillis;

/// Maximum encoded length of an authenticated principal identifier.
pub const MAX_PRINCIPAL_ID_BYTES: usize = 128;

/// A stable, authenticated principal identity used for authorization scoping.
///
/// This is an internal authorization key, not display text. It rejects control
/// characters and whitespace so it remains unambiguous in policy and audit data.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrincipalId(String);

impl PrincipalId {
    /// Validates and constructs an internal principal identity.
    pub fn new(value: impl Into<String>) -> Result<Self, PrincipalIdError> {
        let value = value.into();
        let length = value.len();
        if length == 0 {
            return Err(PrincipalIdError::Empty);
        }
        if length > MAX_PRINCIPAL_ID_BYTES {
            return Err(PrincipalIdError::TooLong {
                maximum: MAX_PRINCIPAL_ID_BYTES,
                actual: length,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(PrincipalIdError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    /// Returns the validated principal identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why an internal principal identifier was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrincipalIdError {
    /// The identifier was empty.
    #[error("principal identifier must not be empty")]
    Empty,
    /// The identifier exceeded its encoded bound.
    #[error("principal identifier is {actual} bytes; maximum is {maximum}")]
    TooLong {
        /// Configured maximum.
        maximum: usize,
        /// Supplied encoded length.
        actual: usize,
    },
    /// The identifier contained whitespace, a control byte, or non-ASCII data.
    #[error("principal identifier must contain only visible ASCII characters")]
    InvalidCharacter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_ids_are_bounded_and_unambiguous() {
        assert!(PrincipalId::new("automation:alice").is_ok());
        assert_eq!(PrincipalId::new(""), Err(PrincipalIdError::Empty));
        assert_eq!(
            PrincipalId::new("alice smith"),
            Err(PrincipalIdError::InvalidCharacter)
        );
        assert!(matches!(
            PrincipalId::new("x".repeat(MAX_PRINCIPAL_ID_BYTES + 1)),
            Err(PrincipalIdError::TooLong { .. })
        ));
    }
}
