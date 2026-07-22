//! Controller-lease request and redacted state wire types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ControlLeaseId, DesktopGeneration, DesktopId, ProtocolVersion, RequestId, Timestamp};

/// Protocol ceiling for a requested controller-lease TTL.
pub const MAX_LEASE_TTL_MS: u32 = 3_600_000;

/// A request to acquire exclusive physical-input control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseAcquireRequest {
    /// Requested protocol version.
    pub protocol_version: ProtocolVersion,
    /// Transport request correlation identifier.
    pub request_id: RequestId,
    /// Target desktop.
    pub desktop_id: DesktopId,
    /// Target desktop lifetime observed by the caller.
    pub desktop_generation: DesktopGeneration,
    /// Requested TTL; omission selects server policy.
    #[schemars(range(min = 1, max = MAX_LEASE_TTL_MS))]
    pub ttl_ms: Option<u32>,
}

impl LeaseAcquireRequest {
    /// Validates version, identifiers, and the requested protocol ceiling.
    pub fn validate(&self) -> Result<(), LeaseValidationError> {
        validate_context(
            self.protocol_version,
            self.request_id,
            self.desktop_id,
            self.desktop_generation,
        )?;
        validate_ttl(self.ttl_ms)
    }
}

/// A request to renew an existing controller lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseRenewRequest {
    /// Requested protocol version.
    pub protocol_version: ProtocolVersion,
    /// Transport request correlation identifier.
    pub request_id: RequestId,
    /// Target desktop.
    pub desktop_id: DesktopId,
    /// Target desktop lifetime observed by the caller.
    pub desktop_generation: DesktopGeneration,
    /// Opaque lease capability being renewed.
    pub lease_id: ControlLeaseId,
    /// Requested replacement TTL; omission selects server policy.
    #[schemars(range(min = 1, max = MAX_LEASE_TTL_MS))]
    pub ttl_ms: Option<u32>,
}

impl LeaseRenewRequest {
    /// Validates version, identifiers, and the requested protocol ceiling.
    pub fn validate(&self) -> Result<(), LeaseValidationError> {
        validate_context(
            self.protocol_version,
            self.request_id,
            self.desktop_id,
            self.desktop_generation,
        )?;
        if self.lease_id.as_uuid().is_nil() {
            return Err(LeaseValidationError::NilIdentifier);
        }
        validate_ttl(self.ttl_ms)
    }
}

/// A request to release an existing controller lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseReleaseRequest {
    /// Requested protocol version.
    pub protocol_version: ProtocolVersion,
    /// Transport request correlation identifier.
    pub request_id: RequestId,
    /// Target desktop.
    pub desktop_id: DesktopId,
    /// Target desktop lifetime observed by the caller.
    pub desktop_generation: DesktopGeneration,
    /// Opaque lease capability being released.
    pub lease_id: ControlLeaseId,
}

impl LeaseReleaseRequest {
    /// Validates version and identifiers.
    pub fn validate(&self) -> Result<(), LeaseValidationError> {
        validate_context(
            self.protocol_version,
            self.request_id,
            self.desktop_id,
            self.desktop_generation,
        )?;
        if self.lease_id.as_uuid().is_nil() {
            return Err(LeaseValidationError::NilIdentifier);
        }
        Ok(())
    }
}

/// Public lease availability without another principal's identity or token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaseAvailability {
    /// No lease/reset transaction exists.
    Vacant,
    /// The authenticated caller owns the active lease.
    HeldByCaller,
    /// Another principal owns the active lease.
    Occupied,
    /// Admission has stopped and cleanup has not started.
    Revoking,
    /// Owned-input cleanup is in progress.
    Resetting,
}

/// A redaction-safe lease state response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseStateView {
    /// Target desktop.
    pub desktop_id: DesktopId,
    /// Authoritative desktop lifetime.
    pub desktop_generation: DesktopGeneration,
    /// Public availability phase.
    pub state: LeaseAvailability,
    /// Lease ID only when the authenticated caller owns it.
    pub lease_id: Option<ControlLeaseId>,
    /// Active expiry; safe occupied views may disclose this without identity.
    pub expires_at: Option<Timestamp>,
}

impl LeaseStateView {
    /// Validates identifier and disclosure invariants.
    pub fn validate(&self) -> Result<(), LeaseValidationError> {
        if self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
            || self.lease_id.is_some_and(|id| id.as_uuid().is_nil())
        {
            return Err(LeaseValidationError::NilIdentifier);
        }
        let shape_is_valid = match self.state {
            LeaseAvailability::Vacant => self.lease_id.is_none() && self.expires_at.is_none(),
            LeaseAvailability::HeldByCaller => self.lease_id.is_some() && self.expires_at.is_some(),
            LeaseAvailability::Occupied => self.lease_id.is_none() && self.expires_at.is_some(),
            LeaseAvailability::Revoking | LeaseAvailability::Resetting => self.lease_id.is_none(),
        };
        if !shape_is_valid {
            return Err(LeaseValidationError::InvalidStateView);
        }
        Ok(())
    }
}

fn validate_context(
    protocol_version: ProtocolVersion,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), LeaseValidationError> {
    if protocol_version.major() != ProtocolVersion::V1_0.major() {
        return Err(LeaseValidationError::UnsupportedMajor);
    }
    if request_id.as_uuid().is_nil()
        || desktop_id.as_uuid().is_nil()
        || desktop_generation.as_uuid().is_nil()
    {
        return Err(LeaseValidationError::NilIdentifier);
    }
    Ok(())
}

fn validate_ttl(ttl_ms: Option<u32>) -> Result<(), LeaseValidationError> {
    if ttl_ms.is_some_and(|ttl| ttl == 0 || ttl > MAX_LEASE_TTL_MS) {
        return Err(LeaseValidationError::TtlOutOfRange);
    }
    Ok(())
}

/// A lease wire-shape validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LeaseValidationError {
    /// The requested protocol major is unsupported.
    #[error("unsupported protocol major")]
    UnsupportedMajor,
    /// A public identifier is UUID nil.
    #[error("lease message contains a nil identifier")]
    NilIdentifier,
    /// A requested TTL is zero or exceeds the protocol ceiling.
    #[error("lease TTL is outside the protocol range")]
    TtlOutOfRange,
    /// Optional state fields do not match the redacted availability state.
    #[error("lease state fields do not match availability")]
    InvalidStateView,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_request_fields_are_rejected() {
        let json = format!(
            r#"{{"protocol_version":{{"major":1,"minor":0}},"request_id":"{}","desktop_id":"{}","desktop_generation":"{}","ttl_ms":1000,"typo":true}}"#,
            RequestId::new(),
            DesktopId::new(),
            DesktopGeneration::new(),
        );
        assert!(serde_json::from_str::<LeaseAcquireRequest>(&json).is_err());
    }

    #[test]
    fn occupied_state_never_contains_a_lease_token() -> Result<(), Box<dyn std::error::Error>> {
        let state = LeaseStateView {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            state: LeaseAvailability::Occupied,
            lease_id: Some(ControlLeaseId::new()),
            expires_at: Some(Timestamp::parse("2026-07-21T00:00:00Z")?),
        };
        assert_eq!(
            state.validate(),
            Err(LeaseValidationError::InvalidStateView)
        );
        Ok(())
    }
}
