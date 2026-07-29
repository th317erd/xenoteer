//! Single source of truth for supported public protocol versions.

use xenoteer_protocol::{ProtocolVersion, VersionError, VersionRange};

/// Inclusive protocol range supported by every version-one transport.
pub(crate) const SERVER_PROTOCOL_RANGE: VersionRange = VersionRange::V1;

/// Negotiates the highest mutually supported protocol version.
pub(crate) fn negotiate(client: VersionRange) -> Result<ProtocolVersion, VersionError> {
    client.negotiate(SERVER_PROTOCOL_RANGE)
}

/// Returns whether a request embeds the exact selected server-supported version.
pub(crate) const fn supports_exact(version: ProtocolVersion) -> bool {
    SERVER_PROTOCOL_RANGE.contains(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_range_is_exact_v1_0() -> Result<(), VersionError> {
        assert_eq!(
            negotiate(VersionRange::new(1, 0, 5)?),
            Ok(ProtocolVersion::V1_0)
        );
        assert!(supports_exact(ProtocolVersion::V1_0));
        assert!(!supports_exact(ProtocolVersion::new(1, 1)));
        Ok(())
    }
}
