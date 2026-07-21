//! X11 backend primitives and executable platform probes for Xenoteer.
//!
//! Connections are intentionally owned by one role. The crate exposes no
//! shared `Arc<Mutex<RustConnection>>` escape hatch.

#![forbid(unsafe_code)]

mod barrier;
pub mod capture;
mod connect;
mod error;
pub mod input;
pub mod keyboard;
mod observe;

pub use barrier::{fake_absolute_motion, query_pointer_barrier};
pub use connect::{
    ExtensionInfo, ExtensionInventory, ExtensionName, OpenedConnection, XConnectionInfo, connect,
};
pub use error::{Result, X11Error};
pub use observe::{
    OBSERVATION_EVENT_CAPACITY, ObservationEventReceiver, ObservationPollHandle,
    ObservationPollThread, PollThreadEvent,
};
