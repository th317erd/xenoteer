//! Public input actor contract smoke tests.

use std::time::Instant;

use xenoteer_protocol::CommandId;
use xenoteer_x11::input::ActionContext;

#[test]
fn action_context_preserves_monotonic_deadline_and_command_identity() {
    let command_id = CommandId::new();
    let deadline = Instant::now();
    let context = ActionContext::new(command_id, Some(deadline));
    assert_eq!(context.command_id, command_id);
    assert_eq!(context.deadline, Some(deadline));
}
