//! Public Rust SDK boundary for Xenoteer.
//!
//! The SDK exposes versioned protocol types, retry-neutral HTTPS/WSS transport,
//! generation-bound domains, bounded events, and verified artifact streaming.

#![forbid(unsafe_code)]

mod client;
mod command;
#[doc(hidden)]
pub mod conformance;
#[path = "lease.rs"]
mod control_lease;
mod domains;
mod events;
mod transport;

pub use client::{Desktop, XenoteerClient};
pub use command::{CommandHandle, CommandSubmission};
pub use control_lease::{
    ControlLease, ControlScopeCallbackAbort, ControlScopeCleanupError, ControlScopeError,
    ControlScopeFuture, MAX_CONTROL_SCOPE_RENEWAL_FAILURE_GRACE, ScopedCommandSubmission,
    ScopedControl,
};
pub use domains::{
    Accessibility, Applications, Artifacts, Capture, Clipboard, ElementHandle, Keyboard, Mouse,
    Viewer, WindowHandle, Windows,
};
pub use events::{
    DEFAULT_EVENT_QUEUE_CAPACITY, EventStream, EventStreamCloseReason, EventStreamItem,
    EventStreamOptions, EventStreamResyncReason, MAX_EVENT_QUEUE_CAPACITY,
};

pub use transport::{
    BaseUri, BearerToken, Client, DEFAULT_CLOSE_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    MAX_ACCESSIBILITY_RESPONSE_BYTES, MAX_RESPONSE_BYTES, MAX_WAIT_TIMEOUT_MS, SdkError,
};

/// Versioned, backend-independent Xenoteer wire types.
pub mod protocol {
    pub use xenoteer_protocol::*;
}

pub use xenoteer_protocol::*;
