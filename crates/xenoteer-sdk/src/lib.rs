//! Public Rust SDK boundary for Xenoteer.
//!
//! Phase 0 establishes the independently Apache-licensed package and its
//! dependency direction. Transport, connection, and ergonomic desktop clients
//! are intentionally deferred until the public v1 protocol is implemented.

#![forbid(unsafe_code)]

/// Versioned, backend-independent Xenoteer wire types.
pub mod protocol {
    pub use xenoteer_protocol::*;
}

pub use xenoteer_protocol::*;
