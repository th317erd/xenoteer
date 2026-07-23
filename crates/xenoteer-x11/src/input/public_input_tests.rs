use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio_util::sync::CancellationToken;
use xenoteer_core::{
    domain::{PointerDelta, RootPoint},
    input::{Effect, LogicalButton, MotionOptions, ScrollAction, ScrollDirection},
};
use xenoteer_protocol::CommandId;
use xenoteer_protocol::{CoordinateSpace, Point, Rect, WindowGeometry, WindowRect};

use super::actor::spawn_test_actor;
use super::backend::BackendEvent;
use super::tests::{MockBackend, MockOperation};
use super::{
    ActionContext, InputEffectEvidence, InputFailureKind, InputOperation, InputPrecondition,
    InputPreconditionFailure, PointerClickRequest, PointerEndpoint, PointerMoveRelativeRequest,
    WindowPointerBoundsPolicy, WindowPointerClickRequest,
};

fn context() -> ActionContext {
    ActionContext::new(CommandId::new(), None)
}

#[test]
fn relative_moves_resolve_from_each_fifo_execution_time_position()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let (handle, join) = spawn_test_actor(4, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let first = handle.try_submit_operation(
        context(),
        InputOperation::PointerMoveRelative(PointerMoveRelativeRequest::new(
            PointerDelta::new(10, 5)?,
            MotionOptions::instant(false),
        )),
        CancellationToken::new(),
    )?;
    let second = handle.try_submit_operation(
        context(),
        InputOperation::PointerMoveRelative(PointerMoveRelativeRequest::new(
            PointerDelta::new(10, -5)?,
            MotionOptions::instant(false),
        )),
        CancellationToken::new(),
    )?;

    assert_eq!(
        first.blocking_recv()??.requested_pointer,
        Some(RootPoint::new(10, 5)?)
    );
    assert_eq!(
        second.blocking_recv()??.requested_pointer,
        Some(RootPoint::new(20, 0)?)
    );
    assert_eq!(
        backend
            .events()
            .into_iter()
            .filter_map(|event| match event {
                BackendEvent::Motion { point, .. } => Some(point),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![RootPoint::new(10, 5)?, RootPoint::new(20, 0)?]
    );
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn compound_click_and_scroll_remain_contiguous_fifo_units() -> Result<(), Box<dyn std::error::Error>>
{
    let backend = MockBackend::new(RootPoint::new(0, 0)?);
    let (handle, join) = spawn_test_actor(4, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let click = handle.try_submit_operation(
        context(),
        InputOperation::PointerClick(PointerClickRequest::new(
            Some(PointerEndpoint::Root(RootPoint::new(4, 4)?)),
            MotionOptions::instant(false),
            LogicalButton::Left,
            2,
            0,
            0,
            1,
        )),
        CancellationToken::new(),
    )?;
    let scroll = handle.try_submit_operation(
        context(),
        InputOperation::PointerScroll(ScrollAction::new(ScrollDirection::Down, 2, 0)?),
        CancellationToken::new(),
    )?;
    assert_eq!(click.blocking_recv()??.completed_units, 2);
    assert_eq!(scroll.blocking_recv()??.completed_units, 2);

    let transitions = backend
        .events()
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::Button {
                button, pressed, ..
            } => Some((button.detail(), pressed)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transitions,
        vec![
            (1, true),
            (1, false),
            (1, true),
            (1, false),
            (5, true),
            (5, false),
            (5, true),
            (5, false)
        ]
    );
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn precondition_runs_after_pointer_resolution_and_before_any_xtest_event()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(3, 3)?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let caller_thread = std::thread::current().id();
    let seen = Arc::new(Mutex::new(None));
    let check_backend = backend.clone();
    let check_seen = Arc::clone(&seen);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::PointerClick(PointerClickRequest::new(
            Some(PointerEndpoint::Relative(PointerDelta::new(2, 2)?)),
            MotionOptions::instant(false),
            LogicalButton::Left,
            1,
            0,
            0,
            0,
        )),
        InputPrecondition::new(move || {
            *check_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((check_backend.counts(), std::thread::current().id()));
            Err(InputPreconditionFailure::TargetStale)
        }),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Ok(_) => unreachable!("precondition rejects"),
        Err(failure) => failure,
    };
    assert_eq!(failure.kind, InputFailureKind::TargetStale);
    let ((event_count, _checks, pointer_calls), precondition_thread) = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or_else(|| unreachable!("precondition ran"));
    assert_eq!(event_count, 0);
    assert!(pointer_calls >= 1);
    assert_ne!(precondition_thread, caller_thread);
    assert!(backend.events().is_empty());
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn window_click_precondition_rejects_before_interpolated_motion()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    let client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(100, 200, 300, 200)?,
    )?;
    backend.set_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
        root,
        WindowGeometry {
            client_rect: client,
            frame_rect: None,
            content_rect: client,
            frame_extents: None,
        },
    )?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
            42,
            CoordinateSpace::WindowClient,
            Point::new(20, 30),
            WindowPointerBoundsPolicy::Reject,
            MotionOptions::interpolated(xenoteer_core::input::MotionCurve::Smooth)?,
            LogicalButton::Left,
            1,
            0,
            0,
            0,
        )),
        InputPrecondition::new(|| Err(InputPreconditionFailure::TargetStale)),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Err(failure) => failure,
        Ok(_) => return Err("stale window click unexpectedly succeeded".into()),
    };
    assert_eq!(failure.kind, InputFailureKind::TargetStale);
    assert_eq!(failure.events_emitted, 0);
    assert_eq!(failure.completed_units, 0);
    assert!(backend.events().is_empty());
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn window_multi_click_revalidates_geometry_identity_and_focus_before_every_button_down()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    let client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(100, 200, 300, 200)?,
    )?;
    backend.set_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
        root,
        WindowGeometry {
            client_rect: client,
            frame_rect: None,
            content_rect: client,
            frame_extents: None,
        },
    )?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let checks = Arc::new(AtomicUsize::new(0));
    let checks_from_precondition = Arc::clone(&checks);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
            42,
            CoordinateSpace::WindowClient,
            Point::new(20, 30),
            WindowPointerBoundsPolicy::Reject,
            MotionOptions::instant(false),
            LogicalButton::Left,
            2,
            7,
            0,
            11,
        )),
        InputPrecondition::new(move || {
            let call = checks_from_precondition.fetch_add(1, Ordering::SeqCst);
            if call < 2 {
                Ok(())
            } else {
                Err(InputPreconditionFailure::FocusLost)
            }
        }),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Ok(_) => unreachable!("second click was emitted after focus was lost"),
        Err(failure) => failure,
    };
    assert_eq!(failure.kind, InputFailureKind::FocusLost);
    assert_eq!(failure.completed_units, 1);
    assert_eq!(failure.events_emitted, 3);
    assert_eq!(checks.load(Ordering::SeqCst), 3);
    assert_eq!(backend.window_geometry_calls(), 3);
    assert_eq!(
        backend
            .events()
            .iter()
            .filter(|event| matches!(event, BackendEvent::Button { .. }))
            .count(),
        2
    );
    assert_eq!(
        backend
            .operations()
            .iter()
            .filter_map(|operation| match operation {
                MockOperation::Delay(duration) => Some(*duration),
                MockOperation::Event(_) | MockOperation::MappingWrite(_) => None,
            })
            .collect::<Vec<_>>(),
        vec![
            std::time::Duration::from_millis(7),
            std::time::Duration::from_millis(11)
        ]
    );
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn window_click_revalidates_geometry_and_focus_after_motion_and_dwell_before_button_down()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    let client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(100, 200, 300, 200)?,
    )?;
    backend.set_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
        root,
        WindowGeometry {
            client_rect: client,
            frame_rect: None,
            content_rect: client,
            frame_extents: None,
        },
    )?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let expected = RootPoint::new(120, 230)?;
    let check_backend = backend.clone();
    let check_expected = expected;
    let checks = Arc::new(AtomicUsize::new(0));
    let checks_from_precondition = Arc::clone(&checks);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
            42,
            CoordinateSpace::WindowClient,
            Point::new(20, 30),
            WindowPointerBoundsPolicy::Reject,
            MotionOptions::instant(false),
            LogicalButton::Left,
            1,
            7,
            0,
            0,
        )),
        InputPrecondition::new(move || {
            let call = checks_from_precondition.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                assert_eq!(check_backend.window_geometry_calls(), 1);
                assert!(check_backend.events().is_empty());
            } else {
                assert_eq!(check_backend.window_geometry_calls(), 2);
                assert!(matches!(
                    check_backend.events().as_slice(),
                    [BackendEvent::Motion { point, delay_ms: 0 }] if *point == check_expected
                ));
                assert!(matches!(
                    check_backend.operations().as_slice(),
                    [
                        MockOperation::Event(BackendEvent::Motion { point, delay_ms: 0 }),
                        MockOperation::Delay(duration),
                    ] if *point == check_expected
                        && *duration == std::time::Duration::from_millis(7)
                ));
            }
            Ok(())
        }),
        CancellationToken::new(),
    )?;
    let outcome = reply.blocking_recv()??;
    assert_eq!(outcome.requested_pointer, Some(expected));
    assert_eq!(outcome.completed_units, 1);
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    assert!(matches!(
        backend.events().as_slice(),
        [
            BackendEvent::Motion { point, delay_ms: 0 },
            BackendEvent::Button { pressed: true, delay_ms: 0, .. },
            BackendEvent::Button { pressed: false, delay_ms: 0, .. },
        ] if *point == expected
    ));
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn window_click_stale_after_motion_and_dwell_never_emits_button_down()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    let client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(100, 200, 300, 200)?,
    )?;
    backend.set_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
        root,
        WindowGeometry {
            client_rect: client,
            frame_rect: None,
            content_rect: client,
            frame_extents: None,
        },
    )?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let check_backend = backend.clone();
    let checks = Arc::new(AtomicUsize::new(0));
    let checks_from_precondition = Arc::clone(&checks);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
            42,
            CoordinateSpace::WindowClient,
            Point::new(20, 30),
            WindowPointerBoundsPolicy::Reject,
            MotionOptions::instant(false),
            LogicalButton::Left,
            1,
            9,
            0,
            0,
        )),
        InputPrecondition::new(move || {
            let call = checks_from_precondition.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                assert_eq!(check_backend.window_geometry_calls(), 1);
                assert!(check_backend.events().is_empty());
                Ok(())
            } else {
                assert_eq!(check_backend.window_geometry_calls(), 2);
                assert!(
                    check_backend
                        .events()
                        .iter()
                        .all(|event| { matches!(event, BackendEvent::Motion { .. }) })
                );
                Err(InputPreconditionFailure::TargetStale)
            }
        }),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Ok(_) => unreachable!("stale target was accepted"),
        Err(failure) => failure,
    };
    assert_eq!(failure.kind, InputFailureKind::TargetStale);
    assert_eq!(failure.completed_units, 0);
    assert_eq!(failure.events_emitted, 1);
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    assert!(
        backend
            .events()
            .iter()
            .all(|event| { matches!(event, BackendEvent::Motion { .. }) })
    );
    let Some(InputEffectEvidence::Journal(journal)) = failure.effects.as_deref() else {
        unreachable!("motion effect journal missing")
    };
    assert_eq!(journal.records().len(), 1);
    assert!(
        journal
            .records()
            .iter()
            .all(|record| { matches!(record.effect(), Effect::PointerMoved { .. }) })
    );
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn window_click_geometry_change_after_motion_fails_before_focus_check_and_button_down()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    for x in [100, 140] {
        let client = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(x, 200, 300, 200)?)?;
        backend.push_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
            root,
            WindowGeometry {
                client_rect: client,
                frame_rect: None,
                content_rect: client,
                frame_extents: None,
            },
        )?);
    }
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let checked = Arc::new(AtomicBool::new(false));
    let checked_from_precondition = Arc::clone(&checked);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(WindowPointerClickRequest::new(
            42,
            CoordinateSpace::WindowClient,
            Point::new(20, 30),
            WindowPointerBoundsPolicy::Reject,
            MotionOptions::instant(false),
            LogicalButton::Left,
            1,
            0,
            0,
            0,
        )),
        InputPrecondition::new(move || {
            checked_from_precondition.store(true, Ordering::SeqCst);
            Ok(())
        }),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Ok(_) => unreachable!("moved window was clicked at stale root coordinates"),
        Err(failure) => failure,
    };
    assert_eq!(failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(failure.completed_units, 0);
    assert_eq!(failure.events_emitted, 1);
    assert_eq!(backend.window_geometry_calls(), 2);
    assert!(checked.load(Ordering::SeqCst));
    assert!(matches!(
        backend.events().as_slice(),
        [BackendEvent::Motion { point, delay_ms: 0 }]
            if *point == RootPoint::new(120, 230)?
    ));
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}

#[test]
fn element_bound_window_click_rejects_changed_root_target_before_motion()
-> Result<(), Box<dyn std::error::Error>> {
    let backend = MockBackend::new(RootPoint::new(1, 1)?);
    let root = WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 800, 600)?)?;
    let moved_client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(140, 200, 300, 200)?,
    )?;
    backend.set_window_geometry(xenoteer_core::window_geometry::WindowGeometryContext::new(
        root,
        WindowGeometry {
            client_rect: moved_client,
            frame_rect: None,
            content_rect: moved_client,
            frame_extents: None,
        },
    )?);
    let (handle, join) = spawn_test_actor(2, {
        let backend = backend.clone();
        move || Ok(backend)
    })?;
    let checked = Arc::new(AtomicBool::new(false));
    let checked_from_precondition = Arc::clone(&checked);
    let request = WindowPointerClickRequest::new(
        42,
        CoordinateSpace::WindowClient,
        Point::new(20, 30),
        WindowPointerBoundsPolicy::Reject,
        MotionOptions::interpolated(xenoteer_core::input::MotionCurve::Linear)?,
        LogicalButton::Left,
        1,
        0,
        0,
        0,
    )
    .with_expected_root_target(RootPoint::new(120, 230)?);
    let reply = handle.try_submit_operation_with_precondition(
        context(),
        InputOperation::WindowPointerClick(request),
        InputPrecondition::new(move || {
            checked_from_precondition.store(true, Ordering::SeqCst);
            Ok(())
        }),
        CancellationToken::new(),
    )?;
    let failure = match reply.blocking_recv()? {
        Err(failure) => failure,
        Ok(_) => return Err("changed root target unexpectedly succeeded".into()),
    };
    assert_eq!(failure.kind, InputFailureKind::PostconditionFailed);
    assert_eq!(failure.events_emitted, 0);
    assert_eq!(failure.completed_units, 0);
    assert!(!checked.load(Ordering::SeqCst));
    assert!(backend.events().is_empty());
    let _ = handle.shutdown().blocking_recv();
    assert_eq!(join.join(), super::InputActorExit::Stopped);
    Ok(())
}
