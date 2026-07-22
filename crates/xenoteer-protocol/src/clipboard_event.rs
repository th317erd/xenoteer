//! Content-free clipboard ownership event contracts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{DesktopGeneration, DesktopId, SelectionName};

/// Public event topic for clipboard or primary-selection owner transitions.
pub const CLIPBOARD_OWNER_CHANGED_TOPIC: &str = "clipboard.owner_changed";

/// One content-free selection-owner transition before global sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClipboardOwnerChangedEvent {
    /// Desktop resource whose selection owner changed.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime that observed the transition.
    pub desktop_generation: DesktopGeneration,
    /// CLIPBOARD or PRIMARY; the selections remain independent.
    pub selection: SelectionName,
    /// Actor-local monotonic revision used only as advisory ordering evidence.
    #[schemars(range(min = 1))]
    pub revision: u64,
    /// Whether Xenoteer's hidden selection window is now the owner.
    pub owned_by_xenoteer: bool,
}

impl ClipboardOwnerChangedEvent {
    /// Rejects nil desktop scope and a wrapped/absent actor revision.
    pub fn validate(self) -> Result<(), ClipboardEventValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(ClipboardEventValidationError::NilIdentifier);
        }
        if self.revision == 0 {
            return Err(ClipboardEventValidationError::Revision);
        }
        Ok(())
    }
}

/// Invalid content-free clipboard event evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClipboardEventValidationError {
    /// Desktop scope contains a nil identifier.
    #[error("clipboard event scope contains a nil identifier")]
    NilIdentifier,
    /// Actor-local revision is zero after initialization or wrap.
    #[error("clipboard event revision is invalid")]
    Revision,
}

#[cfg(test)]
#[path = "clipboard_event_tests.rs"]
mod tests;
