//! Property gates for checked geometry, interpolation, and input state.

use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use xenoteer_core::domain::{PointerDelta, RootPoint, ScreenRect};
use xenoteer_core::input::{
    ActionPurpose, ClickAction, DEFAULT_DOUBLE_CLICK_THRESHOLD_MS, DragAction, HealthEvent,
    InputHealth, InputState, InputStateError, KeyEventKind, KeyPlan, LogicalButton,
    MAX_MOTION_DURATION_MS, MAX_MOTION_EVENTS, MotionCurve, MotionOptions, MotionPolicy,
    PhysicalKey, PoisonReason, ResetReason, ResolvedKey, ScrollAction, ScrollDirection,
    WaypointDurationPolicy, plan_motion, plan_waypoint_motion,
};

fn case_error(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

fn point(x: i32, y: i32) -> Result<RootPoint, TestCaseError> {
    RootPoint::new(x, y).map_err(case_error)
}

fn curve(smooth: bool) -> MotionCurve {
    if smooth {
        MotionCurve::Smooth
    } else {
        MotionCurve::Linear
    }
}

fn reset_reason(index: u8) -> ResetReason {
    match index % 4 {
        0 => ResetReason::CheckedRequestFailed,
        1 => ResetReason::BarrierFailed,
        2 => ResetReason::PostconditionFailed,
        _ => ResetReason::CancelledAfterEffect,
    }
}

fn logical_button(index: u8) -> LogicalButton {
    match index % 9 {
        0 => LogicalButton::Left,
        1 => LogicalButton::Middle,
        2 => LogicalButton::Right,
        3 => LogicalButton::ScrollUp,
        4 => LogicalButton::ScrollDown,
        5 => LogicalButton::ScrollLeft,
        6 => LogicalButton::ScrollRight,
        7 => LogicalButton::Back,
        _ => LogicalButton::Forward,
    }
}

fn scroll_direction(index: u8) -> ScrollDirection {
    match index % 4 {
        0 => ScrollDirection::Up,
        1 => ScrollDirection::Down,
        2 => ScrollDirection::Left,
        _ => ScrollDirection::Right,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn interpolation_has_exact_delay_and_endpoint(
        start_x in -30_000i32..=30_000,
        start_y in -30_000i32..=30_000,
        end_x in -30_000i32..=30_000,
        end_y in -30_000i32..=30_000,
        duration_ms in 0u32..=MAX_MOTION_DURATION_MS,
        sample_rate_hz in 1u16..=240,
        smooth in any::<bool>(),
    ) {
        let start = point(start_x, start_y)?;
        let end = point(end_x, end_y)?;
        let options = MotionOptions::new(
            curve(smooth),
            Some(duration_ms),
            MotionPolicy::new(sample_rate_hz, 1_200, 80, 650).map_err(case_error)?,
            false,
        ).map_err(case_error)?;
        let plan = plan_motion(start, end, options).map_err(case_error)?;
        let delay_sum: u32 = plan.samples().iter().map(|sample| sample.delay_ms()).sum();

        prop_assert!(plan.event_count() <= MAX_MOTION_EVENTS);
        prop_assert!(plan.event_count() <= plan.raw_sample_count());
        if start == end {
            prop_assert!(plan.samples().is_empty());
            prop_assert_eq!(plan.duration_ms(), 0);
            prop_assert_eq!(delay_sum, 0);
        } else {
            prop_assert_eq!(plan.duration_ms(), duration_ms);
            prop_assert_eq!(delay_sum, duration_ms);
            prop_assert_eq!(plan.samples().last().map(|sample| sample.point()), Some(end));
        }
    }

    #[test]
    fn interpolation_is_axis_monotonic_without_overshoot(
        start_x in -30_000i32..=30_000,
        start_y in -30_000i32..=30_000,
        end_x in -30_000i32..=30_000,
        end_y in -30_000i32..=30_000,
        duration_ms in 0u32..=MAX_MOTION_DURATION_MS,
        sample_rate_hz in 1u16..=240,
        smooth in any::<bool>(),
    ) {
        let start = point(start_x, start_y)?;
        let end = point(end_x, end_y)?;
        let plan = plan_motion(
            start,
            end,
            MotionOptions::new(
                curve(smooth),
                Some(duration_ms),
                MotionPolicy::new(sample_rate_hz, 1_200, 80, 650).map_err(case_error)?,
                true,
            ).map_err(case_error)?,
        ).map_err(case_error)?;
        let mut previous = start;
        for sample in plan.samples() {
            let current = sample.point();
            if end.x() >= start.x() {
                prop_assert!(current.x() >= previous.x());
                prop_assert!(current.x() <= end.x());
            } else {
                prop_assert!(current.x() <= previous.x());
                prop_assert!(current.x() >= end.x());
            }
            if end.y() >= start.y() {
                prop_assert!(current.y() >= previous.y());
                prop_assert!(current.y() <= end.y());
            } else {
                prop_assert!(current.y() <= previous.y());
                prop_assert!(current.y() >= end.y());
            }
            previous = current;
        }
    }

    #[test]
    fn duplicate_compaction_never_loses_time(
        delta_x in -3i32..=3,
        delta_y in -3i32..=3,
        duration_ms in 1u32..=1_000,
        sample_rate_hz in 60u16..=240,
        smooth in any::<bool>(),
    ) {
        let start = point(100, 100)?;
        let end = point(100 + delta_x, 100 + delta_y)?;
        let plan = plan_motion(
            start,
            end,
            MotionOptions::new(
                curve(smooth),
                Some(duration_ms),
                MotionPolicy::new(sample_rate_hz, 1_200, 80, 650).map_err(case_error)?,
                true,
            ).map_err(case_error)?,
        ).map_err(case_error)?;
        let delay_sum: u32 = plan.samples().iter().map(|sample| sample.delay_ms()).sum();

        prop_assert!(plan.event_count() <= plan.raw_sample_count());
        prop_assert_eq!(delay_sum, duration_ms);
        prop_assert_eq!(plan.samples().last().map(|sample| sample.point()), Some(end));
    }

    #[test]
    fn proportional_waypoints_preserve_total_and_final_endpoint(
        raw_waypoints in prop::collection::vec((-2_000i32..=2_000, -2_000i32..=2_000), 1..40),
        total_ms in 0u32..=MAX_MOTION_DURATION_MS,
        sample_rate_hz in 1u16..=240,
        smooth in any::<bool>(),
    ) {
        let start = point(0, 0)?;
        let mut waypoints = Vec::with_capacity(raw_waypoints.len());
        for (x, y) in raw_waypoints {
            waypoints.push(point(x, y)?);
        }
        let plan = plan_waypoint_motion(
            start,
            &waypoints,
            curve(smooth),
            MotionPolicy::new(sample_rate_hz, 1_200, 80, 650).map_err(case_error)?,
            false,
            WaypointDurationPolicy::Total(total_ms),
        ).map_err(case_error)?;
        let delay_sum: u32 = plan.samples().iter().map(|sample| sample.delay_ms()).sum();
        let has_motion = waypoints.iter().fold(start, |previous, point| {
            if *point == previous { previous } else { *point }
        }) != start || waypoints.iter().any(|point| *point != start);

        prop_assert!(plan.event_count() <= MAX_MOTION_EVENTS);
        if has_motion {
            prop_assert_eq!(plan.duration_ms(), total_ms);
            prop_assert_eq!(delay_sum, total_ms);
            prop_assert_eq!(
                plan.samples().last().map(|sample| sample.point()),
                waypoints.last().copied()
            );
        } else {
            prop_assert_eq!(plan.duration_ms(), 0);
            prop_assert_eq!(delay_sum, 0);
        }
    }

    #[test]
    fn relative_motion_rejects_every_out_of_range_result(
        start_x in i16::MIN..=i16::MAX,
        start_y in i16::MIN..=i16::MAX,
        dx in any::<i32>(),
        dy in any::<i32>(),
    ) {
        let start = point(i32::from(start_x), i32::from(start_y))?;
        let delta = PointerDelta::new(i64::from(dx), i64::from(dy)).map_err(case_error)?;
        let result_x = i64::from(start_x) + i64::from(dx);
        let result_y = i64::from(start_y) + i64::from(dy);
        let representable = (i64::from(i16::MIN)..=i64::from(i16::MAX)).contains(&result_x)
            && (i64::from(i16::MIN)..=i64::from(i16::MAX)).contains(&result_y);

        prop_assert_eq!(start.checked_add(delta).is_ok(), representable);
    }

    #[test]
    fn screen_admission_never_clamps_without_permission(
        width in 1u32..=2_000,
        height in 1u32..=2_000,
        x in -3_000i32..=3_000,
        y in -3_000i32..=3_000,
    ) {
        let screen = ScreenRect::new(0, 0, width, height).map_err(case_error)?;
        let candidate = point(x, y)?;
        let admitted = screen.admit(candidate, false);
        prop_assert_eq!(admitted.is_ok(), screen.contains(candidate));
        let clamped = screen.admit(candidate, true).map_err(case_error)?;
        prop_assert!(screen.contains(clamped));
    }

    #[test]
    fn generic_chords_stably_partition_and_balance(
        raw_keys in prop::collection::vec((8u8..=u8::MAX, any::<bool>()), 1..=16),
    ) {
        let mut seen = BTreeSet::new();
        let mut keys = Vec::new();
        for (keycode, modifier) in raw_keys {
            if seen.insert(keycode) {
                keys.push(ResolvedKey::new(PhysicalKey::new(keycode).map_err(case_error)?, modifier));
            }
        }
        let plan = KeyPlan::chord(&keys).map_err(case_error)?;
        let expected_press: Vec<ResolvedKey> = keys
            .iter()
            .copied()
            .filter(|key| key.is_modifier())
            .chain(keys.iter().copied().filter(|key| !key.is_modifier()))
            .collect();
        prop_assert_eq!(plan.press_order(), expected_press.as_slice());
        prop_assert_eq!(plan.events().len(), expected_press.len() * 2);
        for (event, expected) in plan.events()[expected_press.len()..]
            .iter()
            .zip(expected_press.iter().rev())
        {
            prop_assert_eq!(event.kind(), KeyEventKind::Release);
            prop_assert_eq!(event.resolved(), *expected);
        }
    }

    #[test]
    fn compound_pointer_actions_preserve_logical_intent_until_execution(
        logical_index in any::<u8>(),
        direction_index in any::<u8>(),
    ) {
        let logical = logical_button(logical_index);
        let movement = plan_motion(
            point(0, 0)?,
            point(10, 10)?,
            MotionOptions::instant(false),
        ).map_err(case_error)?;
        let click = ClickAction::new(
            None,
            logical,
            1,
            0,
            0,
            0,
            DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
        ).map_err(case_error)?;
        let drag = DragAction::new(movement, logical, 0, 0).map_err(case_error)?;
        prop_assert_eq!(click.logical_button(), logical);
        prop_assert_eq!(drag.logical_button(), logical);
        prop_assert_eq!(drag.xtest_event_count(), drag.movement().event_count() + 3);

        let direction = scroll_direction(direction_index);
        let scroll = ScrollAction::new(direction, 1, 0).map_err(case_error)?;
        prop_assert_eq!(scroll.direction(), direction);
        prop_assert_eq!(scroll.logical_button(), direction.logical_button());
    }

    #[test]
    fn health_state_refuses_ordinary_input_until_successful_reset(
        reason_index in any::<u8>(),
        poison in any::<bool>(),
    ) {
        let reason = reset_reason(reason_index);
        let mut state = InputState::new();
        state.begin_action(ActionPurpose::Ordinary).map_err(case_error)?;
        state.submit_pointer_motion(point(1, 1)?).map_err(case_error)?;
        state.fail_batch(reason).map_err(case_error)?;
        let _journal = state.finish_action().map_err(case_error)?;
        prop_assert_eq!(state.health(), InputHealth::ResetRequired(reason));
        prop_assert_eq!(
            state.begin_action(ActionPurpose::Ordinary),
            Err(InputStateError::ActionPurposeNotAllowed {
                purpose: ActionPurpose::Ordinary,
                health: InputHealth::ResetRequired(reason),
            })
        );

        if poison {
            state.begin_action(ActionPurpose::Reset).map_err(case_error)?;
            state.transition_health(HealthEvent::ResetFailed).map_err(case_error)?;
            let _failed_reset_journal = state.finish_action().map_err(case_error)?;
            prop_assert_eq!(state.health(), InputHealth::Poisoned(PoisonReason::ResetFailed));
            prop_assert!(state.begin_action(ActionPurpose::Ordinary).is_err());
            state.begin_action(ActionPurpose::Reset).map_err(case_error)?;
            prop_assert!(state.transition_health(HealthEvent::ResetSucceeded).is_err());
            let _reset_journal = state.finish_action().map_err(case_error)?;
        } else {
            state.begin_action(ActionPurpose::Reset).map_err(case_error)?;
            state.transition_health(HealthEvent::ResetSucceeded).map_err(case_error)?;
            let _reset_journal = state.finish_action().map_err(case_error)?;
            prop_assert_eq!(state.health(), InputHealth::Healthy);
            state.begin_action(ActionPurpose::Ordinary).map_err(case_error)?;
            let _ordinary_journal = state.finish_action().map_err(case_error)?;
        }
    }
}
