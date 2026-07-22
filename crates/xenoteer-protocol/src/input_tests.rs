use serde_json::json;

use crate::{
    Command, CommandEnvelope, CommandId, ControlLeaseId, DesktopGeneration, DesktopId, EffectStage,
    EnvelopeValidationError, InputValidationError, KeyboardChordCommand, KeyboardKeyIdentifier,
    KeyboardNamedKey, KeyboardPressCommand, KeyboardSequenceCommand, KeyboardSequenceStep, Point,
    PointerClickCommand, PointerClickTarget, PointerCurve, PointerDragCommand, PointerDragTarget,
    PointerLogicalButton, PointerMoveRelativeCommand, PointerScrollCommand, PointerScrollDirection,
    ProtocolVersion, RequestId, WindowIdentityHash, WindowPointerBoundsPolicy,
    WindowPointerCoordinateSpace, WindowRef,
};

fn window(desktop_id: DesktopId, generation: DesktopGeneration) -> WindowRef {
    WindowRef {
        desktop_id,
        desktop_generation: generation,
        xid: 42,
        observed_generation: 7,
        identity_hash: WindowIdentityHash::new("a".repeat(64))
            .unwrap_or_else(|_| unreachable!("fixed hash is valid")),
    }
}

fn click(target: PointerClickTarget) -> PointerClickCommand {
    PointerClickCommand {
        target,
        button: PointerLogicalButton::Left,
        count: 2,
        duration_ms: Some(80),
        curve: PointerCurve::Smooth,
        pre_click_dwell_ms: 5,
        press_duration_ms: 20,
        inter_click_interval_ms: 100,
    }
}

#[test]
fn window_click_is_strict_and_bound_to_the_envelope_desktop() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command = Command::PointerClick(click(PointerClickTarget::Window {
        window: window(desktop_id, generation),
        coordinate_space: WindowPointerCoordinateSpace::Client,
        point: Point::new(12, 9),
        activate: true,
        bounds_policy: WindowPointerBoundsPolicy::Reject,
    }));
    assert!(
        CommandEnvelope::new_with_lease(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            ControlLeaseId::new(),
            command,
        )
        .is_ok()
    );

    let foreign = Command::PointerClick(click(PointerClickTarget::Window {
        window: window(DesktopId::new(), generation),
        coordinate_space: WindowPointerCoordinateSpace::Frame,
        point: Point::new(1, 2),
        activate: false,
        bounds_policy: WindowPointerBoundsPolicy::Allow,
    }));
    assert!(matches!(
        CommandEnvelope::new_with_lease(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            ControlLeaseId::new(),
            foreign,
        ),
        Err(EnvelopeValidationError::ReferenceScope)
    ));
}

#[test]
fn nested_input_objects_reject_unknown_fields() {
    let value = json!({
        "type": "pointer_click",
        "target": {
            "kind": "root",
            "point": { "x": 1, "y": 2, "z": 3 }
        },
        "button": "left",
        "count": 1,
        "duration_ms": 0,
        "curve": "instant",
        "pre_click_dwell_ms": 0,
        "press_duration_ms": 0,
        "inter_click_interval_ms": 0
    });
    assert!(serde_json::from_value::<Command>(value).is_err());
}

#[test]
fn keyboard_identifiers_use_stable_snake_case_and_redact_scalars() {
    let named = KeyboardKeyIdentifier::Named {
        name: KeyboardNamedKey::ControlLeft,
    };
    assert_eq!(
        serde_json::to_value(named).unwrap_or_else(|_| unreachable!("identifier serializes")),
        json!({"kind":"named", "name":"control_left"})
    );
    let scalar = KeyboardKeyIdentifier::Scalar { value: '🕵' };
    assert!(!format!("{scalar:?}").contains('🕵'));
    assert_eq!(scalar.validate(), Ok(()));
    assert_eq!(
        KeyboardKeyIdentifier::Scalar { value: '\n' }.validate(),
        Err(InputValidationError::ControlScalar)
    );
}

#[test]
fn chord_and_sequence_aggregate_bounds_are_revalidated() {
    let key = KeyboardKeyIdentifier::Named {
        name: KeyboardNamedKey::Enter,
    };
    let duplicate = KeyboardChordCommand {
        keys: vec![key, key],
        hold_ms: 1,
    };
    assert_eq!(
        duplicate.validate(),
        Err(InputValidationError::DuplicateChordKey)
    );

    let too_long = KeyboardSequenceCommand {
        steps: vec![
            KeyboardSequenceStep {
                keys: vec![key],
                delay_before_ms: 10_000,
                hold_ms: 10_000,
            };
            16
        ],
    };
    assert_eq!(
        too_long.validate(),
        Err(InputValidationError::KeyboardTotalDurationTooLong)
    );
}

#[test]
fn relative_move_accepts_boundaries_and_rejects_invalid_timing() {
    let accepted = PointerMoveRelativeCommand {
        delta: Point::new(i32::MIN, i32::MAX),
        duration_ms: Some(10_000),
        curve: PointerCurve::Linear,
    };
    assert_eq!(accepted.validate(), Ok(()));
    assert_eq!(
        PointerMoveRelativeCommand {
            duration_ms: Some(10_001),
            ..accepted.clone()
        }
        .validate(),
        Err(InputValidationError::PointerDurationTooLong)
    );
    assert_eq!(
        PointerMoveRelativeCommand {
            duration_ms: Some(1),
            curve: PointerCurve::Instant,
            ..accepted
        }
        .validate(),
        Err(InputValidationError::InstantPointerDuration)
    );
}

#[test]
fn click_accepts_exact_boundaries_and_rejects_each_bounded_field() {
    let mut command = click(PointerClickTarget::Current);
    command.count = 5;
    command.duration_ms = Some(10_000);
    command.pre_click_dwell_ms = 10_000;
    command.press_duration_ms = 10_000;
    command.inter_click_interval_ms = 249;
    assert_eq!(command.validate(), Ok(()));

    for invalid in [0, 6] {
        let mut candidate = command.clone();
        candidate.count = invalid;
        assert_eq!(
            candidate.validate(),
            Err(InputValidationError::InvalidClickCount)
        );
    }
    let mut candidate = command.clone();
    candidate.pre_click_dwell_ms = 10_001;
    assert_eq!(
        candidate.validate(),
        Err(InputValidationError::InputDelayTooLong)
    );
    let mut candidate = command.clone();
    candidate.press_duration_ms = 10_001;
    assert_eq!(
        candidate.validate(),
        Err(InputValidationError::InputDelayTooLong)
    );
    let mut candidate = command;
    candidate.inter_click_interval_ms = 250;
    assert_eq!(
        candidate.validate(),
        Err(InputValidationError::InterClickIntervalTooLong)
    );
}

#[test]
fn drag_and_scroll_accept_boundaries_and_reject_overflow_values() {
    let drag = PointerDragCommand {
        target: PointerDragTarget::Relative {
            delta: Point::new(-10, 20),
        },
        button: PointerLogicalButton::Right,
        duration_ms: Some(10_000),
        curve: PointerCurve::Smooth,
        press_dwell_ms: 10_000,
        release_dwell_ms: 10_000,
    };
    assert_eq!(drag.validate(), Ok(()));
    assert_eq!(
        PointerDragCommand {
            release_dwell_ms: 10_001,
            ..drag
        }
        .validate(),
        Err(InputValidationError::InputDelayTooLong)
    );

    assert_eq!(
        PointerScrollCommand {
            direction: PointerScrollDirection::Left,
            count: 1_000,
            interval_ms: 1_000,
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        PointerScrollCommand {
            direction: PointerScrollDirection::Right,
            count: 0,
            interval_ms: 0,
        }
        .validate(),
        Err(InputValidationError::InvalidScrollCount)
    );
    assert_eq!(
        PointerScrollCommand {
            direction: PointerScrollDirection::Down,
            count: 1,
            interval_ms: 1_001,
        }
        .validate(),
        Err(InputValidationError::ScrollIntervalTooLong)
    );
}

#[test]
fn keyboard_commands_accept_boundaries_and_enforce_all_aggregate_limits() {
    assert_eq!(
        KeyboardPressCommand {
            key: KeyboardKeyIdentifier::Raw { keycode: 8 },
            hold_ms: 10_000,
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        KeyboardPressCommand {
            key: KeyboardKeyIdentifier::Raw { keycode: 7 },
            hold_ms: 0,
        }
        .validate(),
        Err(InputValidationError::InvalidRawKeycode)
    );
    assert_eq!(
        KeyboardPressCommand {
            key: KeyboardKeyIdentifier::Named {
                name: KeyboardNamedKey::F24,
            },
            hold_ms: 10_001,
        }
        .validate(),
        Err(InputValidationError::KeyboardDelayTooLong)
    );

    let distinct = (8_u8..24)
        .map(|keycode| KeyboardKeyIdentifier::Raw { keycode })
        .collect::<Vec<_>>();
    assert_eq!(
        KeyboardChordCommand {
            keys: distinct.clone(),
            hold_ms: 10_000,
        }
        .validate(),
        Ok(())
    );
    let mut too_many = distinct;
    too_many.push(KeyboardKeyIdentifier::Raw { keycode: 24 });
    assert_eq!(
        KeyboardChordCommand {
            keys: too_many,
            hold_ms: 0,
        }
        .validate(),
        Err(InputValidationError::InvalidChordLength)
    );

    let one = KeyboardSequenceStep {
        keys: vec![KeyboardKeyIdentifier::Named {
            name: KeyboardNamedKey::Space,
        }],
        delay_before_ms: 0,
        hold_ms: 0,
    };
    assert_eq!(
        KeyboardSequenceCommand {
            steps: vec![one.clone(); 227],
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        KeyboardSequenceCommand {
            steps: vec![one; 228],
        }
        .validate(),
        Err(InputValidationError::TooManyKeyboardEvents)
    );
    assert_eq!(
        KeyboardSequenceCommand { steps: Vec::new() }.validate(),
        Err(InputValidationError::InvalidSequenceLength)
    );
}

#[test]
fn every_new_command_variant_requires_a_lease_and_decodes_strictly() {
    let commands = [
        json!({"type":"pointer_move_relative","delta":{"x":1,"y":2},"duration_ms":0,"curve":"instant"}),
        json!({"type":"pointer_click","target":{"kind":"current"},"button":"left","count":1,"duration_ms":0,"curve":"instant","pre_click_dwell_ms":0,"press_duration_ms":0,"inter_click_interval_ms":0}),
        json!({"type":"pointer_drag","target":{"kind":"root","point":{"x":1,"y":2}},"button":"left","duration_ms":0,"curve":"instant","press_dwell_ms":0,"release_dwell_ms":0}),
        json!({"type":"pointer_scroll","direction":"up","count":1,"interval_ms":0}),
        json!({"type":"keyboard_press","key":{"kind":"named","name":"enter"},"hold_ms":0}),
        json!({"type":"keyboard_chord","keys":[{"kind":"named","name":"control_left"},{"kind":"scalar","value":"v"}],"hold_ms":0}),
        json!({"type":"keyboard_sequence","steps":[{"keys":[{"kind":"raw","keycode":38}],"delay_before_ms":0,"hold_ms":0}]}),
    ];
    for value in commands {
        let command: Command = serde_json::from_value(value)
            .unwrap_or_else(|_| unreachable!("strict fixture decodes"));
        assert_eq!(command.validate(), Ok(()));
        assert!(command.requires_control_lease());
    }

    let unknown = json!({
        "type":"keyboard_sequence",
        "steps":[{"keys":[{"kind":"raw","keycode":38,"extra":true}],"delay_before_ms":0,"hold_ms":0}]
    });
    assert!(serde_json::from_value::<Command>(unknown).is_err());
}

#[test]
fn legacy_raw_input_wire_shapes_remain_unchanged_and_new_stages_are_stable() {
    for value in [
        json!({"type":"pointer_move","target":{"x":1,"y":2},"duration_ms":null,"curve":"linear"}),
        json!({"type":"pointer_button_down","button":1,"allow_redundant":false}),
        json!({"type":"pointer_button_up","button":1,"allow_redundant":false}),
        json!({"type":"keyboard_key_down","keycode":38,"allow_redundant":false}),
        json!({"type":"keyboard_key_up","keycode":38,"allow_redundant":false}),
        json!({"type":"input_reset"}),
    ] {
        let decoded: Command = serde_json::from_value(value.clone())
            .unwrap_or_else(|_| unreachable!("legacy fixture decodes"));
        assert_eq!(
            serde_json::to_value(decoded)
                .unwrap_or_else(|_| unreachable!("legacy command serializes")),
            value
        );
    }
    assert_eq!(
        serde_json::to_value(EffectStage::PointerClicked)
            .unwrap_or_else(|_| unreachable!("stage serializes")),
        json!("pointer_clicked")
    );
    assert_eq!(
        serde_json::to_value(EffectStage::KeyboardActionCompleted)
            .unwrap_or_else(|_| unreachable!("stage serializes")),
        json!("keyboard_action_completed")
    );
}
