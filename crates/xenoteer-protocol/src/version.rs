//! Protocol version types and negotiation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A negotiated protocol major/minor pair.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    major: u16,
    minor: u16,
}

impl ProtocolVersion {
    /// Protocol version 1.0.
    pub const V1_0: Self = Self { major: 1, minor: 0 };

    /// Creates a protocol version.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Returns the major version.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Returns the minor version.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }
}

/// An inclusive range of client-supported minors within one major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionRange {
    major: u16,
    min_minor: u16,
    max_minor: u16,
}

impl VersionRange {
    /// Creates a valid inclusive range.
    pub fn new(major: u16, min_minor: u16, max_minor: u16) -> Result<Self, VersionError> {
        if min_minor > max_minor {
            return Err(VersionError::ReversedMinorRange);
        }
        Ok(Self {
            major,
            min_minor,
            max_minor,
        })
    }

    /// Negotiates the highest mutually supported version.
    pub fn negotiate(self, server: Self) -> Result<ProtocolVersion, VersionError> {
        self.validate()?;
        server.validate()?;
        if self.major != server.major {
            return Err(VersionError::UnsupportedMajor);
        }
        let minimum = self.min_minor.max(server.min_minor);
        let maximum = self.max_minor.min(server.max_minor);
        if minimum > maximum {
            return Err(VersionError::NoSharedMinor);
        }
        Ok(ProtocolVersion::new(self.major, maximum))
    }

    /// Validates a value obtained through deserialization.
    pub fn validate(self) -> Result<(), VersionError> {
        if self.min_minor > self.max_minor {
            return Err(VersionError::ReversedMinorRange);
        }
        Ok(())
    }
}

/// A protocol negotiation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VersionError {
    /// The inclusive minor range is reversed.
    #[error("minimum protocol minor exceeds maximum")]
    ReversedMinorRange,
    /// The peers do not support the same major version.
    #[error("unsupported protocol major")]
    UnsupportedMajor,
    /// The peers' minor-version ranges do not intersect.
    #[error("no mutually supported protocol minor")]
    NoSharedMinor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_selects_highest_common_minor() -> Result<(), VersionError> {
        let client = VersionRange::new(1, 0, 4)?;
        let server = VersionRange::new(1, 2, 3)?;
        assert_eq!(client.negotiate(server), Ok(ProtocolVersion::new(1, 3)));
        Ok(())
    }
}
