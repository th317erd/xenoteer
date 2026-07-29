//! Authenticated server and desktop discovery response.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CapabilityReport, CapabilityReportError, DesktopGeneration, DesktopId, ProtocolVersion,
    Timestamp, VersionRange,
};

/// Maximum UTF-8 byte length of a server package version.
pub const MAX_SERVER_VERSION_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a stable desktop reason code.
pub const MAX_DESKTOP_REASON_CODE_BYTES: usize = 128;

/// Coarse externally visible desktop lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesktopState {
    /// Process composition is still starting.
    Booting,
    /// Required capability probes are running.
    Probing,
    /// Every required capability probe passed.
    Ready,
    /// Required capabilities passed but an optional capability is unavailable.
    Degraded,
    /// New work is refused while shutdown cleanup runs.
    Draining,
    /// Shutdown completed.
    Stopped,
    /// A critical invariant or subsystem failed.
    Failed,
}

/// Current generation-bound desktop summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DesktopStatus {
    /// Stable desktop identifier.
    pub id: DesktopId,
    /// Current lifetime, once a desktop session exists.
    pub generation: Option<DesktopGeneration>,
    /// Coarse lifecycle state.
    pub state: DesktopState,
    /// Stable safe reason code for non-nominal state.
    #[schemars(
        length(min = 1, max = MAX_DESKTOP_REASON_CODE_BYTES),
        regex(pattern = "^[a-z0-9._-]+$")
    )]
    pub reason_code: Option<String>,
}

impl DesktopStatus {
    /// Validates identifiers and the bounded stable reason code.
    pub fn validate(&self) -> Result<(), StatusValidationError> {
        if self.id.as_uuid().is_nil()
            || self
                .generation
                .is_some_and(|generation| generation.as_uuid().is_nil())
        {
            return Err(StatusValidationError::NilIdentifier);
        }
        if self.reason_code.as_deref().is_some_and(|reason| {
            reason.is_empty()
                || reason.len() > MAX_DESKTOP_REASON_CODE_BYTES
                || !reason.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                })
        }) {
            return Err(StatusValidationError::ReasonCode);
        }
        Ok(())
    }
}

/// Authenticated status and protocol-discovery response.
///
/// Response objects intentionally accept additive fields so older SDKs can
/// decode newer compatible server responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusResponse {
    /// Server package version.
    #[schemars(length(min = 1, max = MAX_SERVER_VERSION_BYTES))]
    pub server_version: String,
    /// Oldest supported exact protocol version.
    pub protocol_min: ProtocolVersion,
    /// Newest supported exact protocol version.
    pub protocol_max: ProtocolVersion,
    /// Server wall-clock observation used to derive absolute request deadlines.
    pub server_time: Timestamp,
    /// Current desktop identity and readiness.
    pub desktop: DesktopStatus,
    /// Current live capability evidence.
    pub capabilities: CapabilityReport,
}

impl StatusResponse {
    /// Validates the bounded response and its advertised protocol range.
    pub fn validate(&mut self) -> Result<(), StatusValidationError> {
        if self.server_version.is_empty()
            || self.server_version.len() > MAX_SERVER_VERSION_BYTES
            || self.server_version.chars().any(char::is_control)
        {
            return Err(StatusValidationError::ServerVersion);
        }
        VersionRange::new(
            self.protocol_min.major(),
            self.protocol_min.minor(),
            self.protocol_max.minor(),
        )
        .map_err(|_| StatusValidationError::ProtocolRange)?;
        if self.protocol_min.major() != self.protocol_max.major() {
            return Err(StatusValidationError::ProtocolRange);
        }
        self.desktop.validate()?;
        self.capabilities.validate()?;
        Ok(())
    }
}

/// Invalid authenticated status response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StatusValidationError {
    /// The package version is empty, unsafe, or over its bound.
    #[error("server version is invalid")]
    ServerVersion,
    /// The advertised version endpoints do not form one inclusive range.
    #[error("server protocol range is invalid")]
    ProtocolRange,
    /// A desktop identifier is nil.
    #[error("status contains a nil desktop identifier")]
    NilIdentifier,
    /// The stable desktop reason code is invalid.
    #[error("desktop reason code is invalid")]
    ReasonCode,
    /// The capability report is invalid.
    #[error(transparent)]
    Capabilities(#[from] CapabilityReportError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Result<StatusResponse, Box<dyn std::error::Error>> {
        Ok(StatusResponse {
            server_version: "0.1.0".to_owned(),
            protocol_min: ProtocolVersion::V1_0,
            protocol_max: ProtocolVersion::V1_0,
            server_time: Timestamp::parse("2026-07-22T00:00:00Z")?,
            desktop: DesktopStatus {
                id: DesktopId::new(),
                generation: Some(DesktopGeneration::new()),
                state: DesktopState::Ready,
                reason_code: None,
            },
            capabilities: CapabilityReport::checked(Vec::new())?,
        })
    }

    #[test]
    fn status_is_additive_and_validated() -> Result<(), Box<dyn std::error::Error>> {
        let response = response()?;
        let mut value = serde_json::to_value(&response)?;
        value["future"] = serde_json::json!({"nested": true});
        value["desktop"]["future"] = serde_json::json!(true);
        let mut decoded: StatusResponse = serde_json::from_value(value)?;
        decoded.validate()?;
        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn status_rejects_reversed_or_cross_major_ranges() -> Result<(), Box<dyn std::error::Error>> {
        let mut reversed = response()?;
        reversed.protocol_min = ProtocolVersion::new(1, 1);
        assert_eq!(
            reversed.validate(),
            Err(StatusValidationError::ProtocolRange)
        );

        let mut cross_major = response()?;
        cross_major.protocol_max = ProtocolVersion::new(2, 0);
        assert_eq!(
            cross_major.validate(),
            Err(StatusValidationError::ProtocolRange)
        );
        Ok(())
    }
}
