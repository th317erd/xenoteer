//! Single-owner physical-input actor and its public command contract.

mod actor;
mod backend;
mod command;
mod execute;
mod keyboard_action;
mod keyboard_model;
mod outcome;

pub use actor::{
    DEFAULT_INPUT_QUEUE_CAPACITY, InputActorExit, InputActorHandle, InputActorJoin,
    InputSubmitError, spawn_input_actor,
};
pub use command::{ActionContext, InputCommand, InputOperation, PointerMoveRequest};
pub use keyboard_action::{
    KeyboardAction, KeyboardActionError, KeyboardSequenceStep, MAX_KEYBOARD_ACTION_EVENTS,
    MAX_KEYBOARD_CHORD_KEYS, MAX_KEYBOARD_DELAY_MS, MAX_KEYBOARD_SEQUENCE_STEPS,
    MAX_KEYBOARD_TOTAL_DURATION_MS, MAX_PHYSICAL_TEXT_SCALARS, PhysicalTextMode,
};
pub use outcome::{
    ActorThreadState, CleanupReport, ControlOutcome, InputCleanupEvidence, InputEffectEvidence,
    InputFailure, InputFailureKind, InputHealthSnapshot, InputOutcome, InputOutcomeKind,
    KeyboardBindingEvidence, KeyboardModelDiagnostics, KeyboardOutcomeEvidence,
};

#[cfg(test)]
mod tests;
