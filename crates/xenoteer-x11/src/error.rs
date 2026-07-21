//! Typed errors produced by the X11 adapter.

use x11rb::errors::ReplyError;

/// Result alias for X11 adapter operations.
pub type Result<T> = std::result::Result<T, X11Error>;

/// Failures reported by the X11 adapter without exposing protocol internals to
/// higher layers.
#[derive(Debug, thiserror::Error)]
pub enum X11Error {
    /// The display connection or authentication handshake failed.
    #[error("cannot connect to X display: {0}")]
    Connect(String),
    /// A request could not be written or the connection was lost.
    #[error("X11 connection failed: {0}")]
    Connection(String),
    /// The server rejected a request or returned an invalid reply.
    #[error("X11 request failed: {0}")]
    Reply(String),
    /// A required extension was absent.
    #[error("required X11 extension is absent: {0}")]
    MissingExtension(&'static str),
    /// Display setup data was missing or inconsistent.
    #[error("invalid X11 setup: {0}")]
    InvalidSetup(&'static str),
    /// A coordinate cannot be represented by the XTEST protocol.
    #[error("XTEST coordinate is outside signed 16-bit range: ({x}, {y})")]
    CoordinateOutOfRange {
        /// Root X coordinate.
        x: i32,
        /// Root Y coordinate.
        y: i32,
    },
    /// An XTEST server-side delay exceeded the shared protocol ceiling.
    #[error("XTEST delay {requested}ms exceeds the {max}ms ceiling")]
    DelayOutOfRange {
        /// Requested server-side delay in milliseconds.
        requested: u32,
        /// Maximum admitted delay in milliseconds.
        max: u32,
    },
    /// A capture format or reply was unsupported or malformed.
    #[error("X11 pixel decode failed: {0}")]
    Pixel(String),
    /// The visual requires colormap semantics that the Phase 0 decoder does not implement.
    #[error("unsupported X11 visual class {visual_class}; only TrueColor is supported")]
    UnsupportedVisualClass {
        /// Core X11 visual class numeric value.
        visual_class: u8,
    },
    /// Poll-loop setup or execution failed.
    #[error("X11 event poll loop failed: {0}")]
    Poll(String),
    /// A native keyboard model could not be constructed.
    #[error("XKB keyboard model failed: {0}")]
    Keyboard(String),
    /// A platform worker thread panicked.
    #[error("X11 platform worker panicked")]
    WorkerPanicked,
}

pub(crate) fn classify_reply_error(error: ReplyError) -> X11Error {
    match error {
        ReplyError::ConnectionError(error) => X11Error::Connection(error.to_string()),
        ReplyError::X11Error(error) => X11Error::Reply(format!("{error:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use x11rb::errors::{ConnectionError, ReplyError};

    use super::{X11Error, classify_reply_error};

    #[test]
    fn reply_connection_errors_remain_connection_failures() {
        let error = ReplyError::ConnectionError(ConnectionError::IoError(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test disconnect",
        )));
        assert!(matches!(
            classify_reply_error(error),
            X11Error::Connection(_)
        ));
    }
}
