//! Public Rust SDK boundary for Xenoteer.
//!
//! The SDK exposes the versioned protocol types and a small, retry-neutral HTTP
//! transport for lease and command operations.

#![forbid(unsafe_code)]

mod transport;

pub use transport::{
    BaseUri, BearerToken, Client, DEFAULT_REQUEST_TIMEOUT, MAX_RESPONSE_BYTES, MAX_WAIT_TIMEOUT_MS,
    SdkError,
};

/// Versioned, backend-independent Xenoteer wire types.
pub mod protocol {
    pub use xenoteer_protocol::*;
}

pub use xenoteer_protocol::*;
