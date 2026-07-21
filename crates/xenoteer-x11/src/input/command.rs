//! Queue-level input command types.

use std::time::Instant;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xenoteer_core::input::InputAction;
use xenoteer_protocol::CommandId;

use super::{InputFailure, InputOutcome, KeyboardAction};

/// One FIFO-serialized pointer/button or unresolved keyboard operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOperation {
    /// Already validated backend-independent pointer/button action.
    Pointer(InputAction),
    /// Bounded keyboard intent resolved only by the actor-owned live model.
    Keyboard(KeyboardAction),
}

/// Identity and monotonic execution deadline carried through the actor queue.
#[derive(Debug, Clone, Copy)]
pub struct ActionContext {
    /// Public command identifier used for result correlation.
    pub command_id: CommandId,
    /// Optional monotonic deadline; wall-clock changes cannot extend it.
    pub deadline: Option<Instant>,
}

impl ActionContext {
    /// Builds action context for one admitted command.
    #[must_use]
    pub const fn new(command_id: CommandId, deadline: Option<Instant>) -> Self {
        Self {
            command_id,
            deadline,
        }
    }
}

/// One validated ordinary command sent through the bounded FIFO queue.
#[derive(Debug)]
pub struct InputCommand {
    /// Queue-level identity and deadline.
    pub context: ActionContext,
    /// Pointer action or unresolved keyboard intent.
    pub operation: InputOperation,
    /// Cooperative cancellation signal checked only at documented safe boundaries.
    pub cancellation: CancellationToken,
    /// Exactly one terminal actor result.
    pub reply: oneshot::Sender<Result<InputOutcome, InputFailure>>,
}
