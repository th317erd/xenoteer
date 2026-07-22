use super::*;

fn failure(kind: InputFailureKind) -> InputFailure {
    InputFailure {
        command_id: Some(CommandId::new()),
        kind,
        events_emitted: 0,
        completed_units: 0,
        progress_known: true,
        requested_pointer: None,
        last_observed_pointer: None,
        observed_logical_buttons_1_to_5: None,
        button_observation_partial: false,
        effects: None,
        cleanup: None,
        keyboard: None,
    }
}

#[test]
fn compound_input_stages_are_precise() {
    assert_eq!(
        InputStage::PointerClick.after_effect(),
        EffectStage::PointerClicked
    );
    assert_eq!(
        InputStage::PointerDrag.after_effect(),
        EffectStage::PointerDragged
    );
    assert_eq!(
        InputStage::PointerScroll.after_effect(),
        EffectStage::PointerScrolled
    );
    assert_eq!(
        InputStage::KeyboardAction.after_effect(),
        EffectStage::KeyboardActionCompleted
    );
}

#[test]
fn partial_input_effects_report_the_last_proven_primitive_not_the_requested_compound() {
    let point = RootPoint::new(4, 5).unwrap_or_else(|_| unreachable!("fixed point is valid"));
    let button =
        PhysicalButton::new(1).unwrap_or_else(|_| unreachable!("fixed physical button is valid"));
    assert_eq!(
        input_effect_stage(Effect::PointerMoved { point }),
        EffectStage::PointerMoved
    );
    assert_eq!(
        input_effect_stage(Effect::ButtonPressed { button }),
        EffectStage::ButtonPressed
    );
    assert_eq!(
        input_effect_stage(Effect::ButtonReleased { button }),
        EffectStage::ButtonReleased
    );
}

#[test]
fn window_precondition_failures_keep_prior_activation_effect() {
    let stale = input_failure(
        failure(InputFailureKind::TargetStale),
        InputStage::PointerClick,
        EffectStage::WindowStateChanged,
    );
    let RuntimeResult::Failure(stale) = stale else {
        unreachable!("stale input unexpectedly succeeded");
    };
    assert_eq!(stale.code, ErrorCode::StaleReference);
    assert_eq!(stale.effect_stage, EffectStage::WindowStateChanged);

    let focus = input_failure(
        failure(InputFailureKind::FocusLost),
        InputStage::PointerClick,
        EffectStage::WindowStateChanged,
    );
    let RuntimeResult::Failure(focus) = focus else {
        unreachable!("focus-lost input unexpectedly succeeded");
    };
    assert_eq!(focus.code, ErrorCode::UnsupportedByTarget);
    assert_eq!(focus.effect_stage, EffectStage::WindowStateChanged);
}

#[test]
fn protocol_buttons_and_motion_curves_map_without_backend_guessing() {
    assert_eq!(
        input_logical_button(PointerLogicalButton::Left),
        LogicalButton::Left
    );
    assert_eq!(
        input_logical_button(PointerLogicalButton::Back),
        LogicalButton::Back
    );
    assert_eq!(
        input_logical_button(PointerLogicalButton::Forward),
        LogicalButton::Forward
    );
    let options = input_motion_options(PointerCurve::Smooth, Some(25), MotionPolicy::default())
        .unwrap_or_else(|_| unreachable!("fixed options are valid"));
    assert_eq!(options.curve(), MotionCurve::Smooth);
    assert_eq!(options.duration_ms(), Some(25));
}

#[test]
fn window_click_requires_near_effect_focus_even_without_activation_request() {
    let target = WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid: 77,
        observed_generation: 2,
        identity_hash: xenoteer_protocol::WindowIdentityHash::new("c".repeat(64))
            .unwrap_or_else(|_| unreachable!("fixed hash is valid")),
    };
    let precondition = window_click_precondition_spec(target.clone());
    assert_eq!(precondition.target, target);
    assert!(precondition.require_focus);
}
