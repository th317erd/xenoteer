use xenoteer_protocol::{
    Command, DesktopGeneration, DesktopId, KeyboardChordCommand, KeyboardKeyIdentifier,
    KeyboardNamedKey, KeyboardPressCommand, KeyboardSequenceCommand, KeyboardSequenceStep, Point,
    PointerClickCommand, PointerClickTarget, PointerCurve, PointerDragCommand, PointerDragTarget,
    PointerLogicalButton, PointerMoveRelativeCommand, PointerScrollCommand, PointerScrollDirection,
    WindowIdentityHash, WindowPointerBoundsPolicy, WindowPointerCoordinateSpace, WindowRef,
};

use crate::{Grant, command_grant_requirement};

fn click(target: PointerClickTarget) -> Command {
    Command::PointerClick(PointerClickCommand {
        target,
        button: PointerLogicalButton::Left,
        count: 1,
        duration_ms: Some(0),
        curve: PointerCurve::Instant,
        pre_click_dwell_ms: 0,
        press_duration_ms: 0,
        inter_click_interval_ms: 0,
    })
}

#[test]
fn current_and_root_clicks_require_only_input_control() {
    for command in [
        click(PointerClickTarget::Current),
        click(PointerClickTarget::Root {
            point: Point::new(1, 2),
        }),
    ] {
        assert_eq!(
            command_grant_requirement(&command).grants(),
            &[Grant::InputControl]
        );
    }
}

#[test]
fn exact_window_click_requires_input_and_window_control_all_of() {
    let command = click(PointerClickTarget::Window {
        window: WindowRef {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            xid: 9,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new("b".repeat(64))
                .unwrap_or_else(|_| unreachable!("fixed hash is valid")),
        },
        coordinate_space: WindowPointerCoordinateSpace::Client,
        point: Point::new(4, 5),
        activate: true,
        bounds_policy: WindowPointerBoundsPolicy::Clamp,
    });
    assert_eq!(
        command_grant_requirement(&command).grants(),
        &[Grant::InputControl, Grant::WindowControl]
    );
}

#[test]
fn every_other_compound_input_command_remains_input_only() {
    let enter = KeyboardKeyIdentifier::Named {
        name: KeyboardNamedKey::Enter,
    };
    let commands = vec![
        Command::PointerMoveRelative(PointerMoveRelativeCommand {
            delta: Point::new(1, -2),
            duration_ms: Some(10),
            curve: PointerCurve::Linear,
        }),
        Command::PointerDrag(PointerDragCommand {
            target: PointerDragTarget::Relative {
                delta: Point::new(5, 6),
            },
            button: PointerLogicalButton::Left,
            duration_ms: Some(20),
            curve: PointerCurve::Smooth,
            press_dwell_ms: 0,
            release_dwell_ms: 0,
        }),
        Command::PointerScroll(PointerScrollCommand {
            direction: PointerScrollDirection::Down,
            count: 2,
            interval_ms: 1,
        }),
        Command::KeyboardPress(KeyboardPressCommand {
            key: enter,
            hold_ms: 0,
        }),
        Command::KeyboardChord(KeyboardChordCommand {
            keys: vec![enter],
            hold_ms: 0,
        }),
        Command::KeyboardSequence(KeyboardSequenceCommand {
            steps: vec![KeyboardSequenceStep {
                keys: vec![enter],
                delay_before_ms: 0,
                hold_ms: 0,
            }],
        }),
    ];
    for command in commands {
        assert_eq!(
            command_grant_requirement(&command).grants(),
            &[Grant::InputControl]
        );
    }
}
