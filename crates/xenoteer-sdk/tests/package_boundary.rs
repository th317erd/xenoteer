//! Compile-time checks for the independently licensed SDK package boundary.

use xenoteer_sdk::{ProtocolVersion, VersionRange, protocol};

#[test]
fn sdk_exposes_protocol_without_server_implementation() {
    let supported = VersionRange::new(1, 0, 0);
    assert_eq!(
        supported.and_then(|range| range.negotiate(range)),
        Ok(ProtocolVersion::V1_0)
    );

    let _: Option<protocol::CommandEnvelope> = None;
}
