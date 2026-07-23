//! Real Xvfb/XTEST/xkbcommon backend proofs.

use std::thread;
use std::time::{Duration, Instant};

#[cfg(feature = "native-xkbcommon")]
use tokio_util::sync::CancellationToken;
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{
    ConnectionExt as _, EventType, ID, MapPart, NKNDetail, SelectEventsAux,
    SelectEventsAuxNewKeyboardNotify,
};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Rectangle, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
#[cfg(feature = "native-xkbcommon")]
use xenoteer_protocol::CommandId;
use xenoteer_protocol::{WindowGeometryRequest, WindowGeometryTarget, WindowScreenBoundsPolicy};
use xenoteer_x11::{
    DesktopProbeExpectation, ExtensionName, OBSERVATION_EVENT_CAPACITY, ObservationActorEvent,
    ObservationActorExit, ObservationActorState, ObservationPollThread, PollThreadEvent,
    RawWindowControlOperation, RawWindowControlOutcome, RawWindowControlRequest,
    WindowControlActorExit, WindowControlActorState, connect, fake_absolute_motion, probe_desktop,
    query_pointer_barrier, spawn_observation_actor, spawn_window_control_actor,
};
#[cfg(feature = "native-xkbcommon")]
use xenoteer_x11::{
    input::{ActionContext, InputActorExit, InputOutcomeKind, KeyboardAction, spawn_input_actor},
    keyboard::{KeyIdentifier, NamedKey},
};

fn display() -> Result<String, Box<dyn std::error::Error>> {
    Ok(std::env::var("DISPLAY").map_err(|_| {
        "DISPLAY is required; run tests/platform/run-x11-spikes.sh or set an authenticated Xvfb"
    })?)
}

#[test]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn core_roundtrip_inventories_extensions_and_barriers() -> Result<(), Box<dyn std::error::Error>> {
    let opened = connect(&display()?)?;
    let focus = opened.core_roundtrip()?;
    assert_ne!(focus, u32::MAX);
    assert!(
        opened
            .info
            .extensions
            .require(ExtensionName::XTest)?
            .present
    );
    assert!(
        opened
            .info
            .extensions
            .require(ExtensionName::XKeyboard)?
            .present
    );

    let pointer = query_pointer_barrier(&opened.connection, opened.info.root)?;
    assert!(pointer.same_screen);
    Ok(())
}

#[test]
#[ignore = "requires the authenticated Phase-2 Xvfb/XFCE profile"]
fn desktop_probe_proves_ewmh_lifecycle_workspace_and_capture()
-> Result<(), Box<dyn std::error::Error>> {
    let evidence = probe_desktop(
        &display()?,
        DesktopProbeExpectation {
            width_px: 1_920,
            height_px: 1_080,
            depth: 24,
            dpi: 96,
        },
    )?;
    assert_ne!(evidence.supporting_wm_window, 0);
    assert!(evidence.supported_atom_count >= 5);
    assert_eq!(evidence.workspace_count, 1);
    assert_eq!(evidence.current_workspace, 0);
    assert_eq!((evidence.dpi_x, evidence.dpi_y), (96, 96));
    assert_eq!(evidence.capture_bytes, 4);
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb with XTEST; run tests/platform/run-x11-spikes.sh"]
fn xtest_motion_is_delivered_and_same_connection_barrier_observes_it()
-> Result<(), Box<dyn std::error::Error>> {
    let receiver = connect(&display()?)?;
    let screen = &receiver.connection.setup().roots[receiver.info.screen_index];
    let window = receiver.connection.generate_id()?;
    receiver.connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        320,
        240,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(
            EventMask::POINTER_MOTION
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::EXPOSURE,
        ),
    )?;
    receiver.connection.map_window(window)?;
    receiver.connection.flush()?;

    let sender = connect(&display()?)?;
    let observed = fake_absolute_motion(&sender.connection, sender.info.root, 40, 50, 0)?;
    assert_eq!((observed.root_x, observed.root_y), (40, 50));

    let mut received_motion = false;
    for _ in 0..20 {
        if let Some(x11rb::protocol::Event::MotionNotify(event)) =
            receiver.connection.poll_for_event()?
        {
            assert_eq!((event.root_x, event.root_y), (40, 50));
            received_motion = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        received_motion,
        "recorder connection did not receive MotionNotify"
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated 24-depth Xvfb; run tests/platform/run-x11-spikes.sh"]
fn core_get_image_decodes_live_depth_24_bpp_32_color_bars() -> Result<(), Box<dyn std::error::Error>>
{
    let opened = connect(&display()?)?;
    assert_eq!(
        opened.info.root_depth, 24,
        "spike requires the declared depth-24 profile"
    );
    let pixmap_format = opened
        .connection
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == opened.info.root_depth)
        .ok_or("root depth has no pixmap format")?;
    assert_eq!(
        pixmap_format.bits_per_pixel, 32,
        "spike must exercise the depth-24/bpp-32 gotcha"
    );
    let pixmap = opened.connection.generate_id()?;
    opened
        .connection
        .create_pixmap(opened.info.root_depth, pixmap, opened.info.root, 5, 1)?;
    let gc = opened.connection.generate_id()?;
    opened
        .connection
        .create_gc(gc, pixmap, &CreateGCAux::new())?;
    for (x, color) in [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff, 0]
        .into_iter()
        .enumerate()
    {
        opened
            .connection
            .change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        opened.connection.poly_fill_rectangle(
            pixmap,
            gc,
            &[Rectangle {
                x: i16::try_from(x)?,
                y: 0,
                width: 1,
                height: 1,
            }],
        )?;
    }
    opened.connection.flush()?;

    let decoded = xenoteer_x11::capture::get_image_bgra8(
        &opened.connection,
        &opened.info,
        pixmap,
        0,
        0,
        5,
        1,
    )?;
    assert_eq!(
        decoded,
        vec![
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255,
        ]
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn poll_thread_observes_map_and_stops_through_explicit_waker()
-> Result<(), Box<dyn std::error::Error>> {
    let observer = connect(&display()?)?;
    observer.select_root_events(EventMask::SUBSTRUCTURE_NOTIFY)?;
    let root = observer.info.root;
    let screen_index = observer.info.screen_index;
    let (handle, events) = ObservationPollThread::spawn(observer.connection)?;

    let producer = connect(&display()?)?;
    let screen = &producer.connection.setup().roots[screen_index];
    let window = producer.connection.generate_id()?;
    producer.connection.create_window(
        screen.root_depth,
        window,
        root,
        10,
        10,
        80,
        60,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new(),
    )?;
    producer.connection.map_window(window)?;
    producer.connection.flush()?;

    let mut mapped = false;
    for _ in 0..4 {
        let event = events.recv_timeout(Duration::from_secs(2))?;
        if event == (PollThreadEvent::Map { window }) {
            mapped = true;
            break;
        }
    }
    assert!(mapped, "poll worker did not observe MapNotify");
    handle.shutdown()?;
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn poll_thread_remains_shutdown_responsive_during_bounded_queue_flood()
-> Result<(), Box<dyn std::error::Error>> {
    let observer = connect(&display()?)?;
    observer.select_root_events(EventMask::SUBSTRUCTURE_NOTIFY)?;
    let root = observer.info.root;
    let screen_index = observer.info.screen_index;
    let (handle, _events) = ObservationPollThread::spawn(observer.connection)?;

    let producer = connect(&display()?)?;
    let screen = &producer.connection.setup().roots[screen_index];
    for _ in 0..OBSERVATION_EVENT_CAPACITY * 4 {
        let window = producer.connection.generate_id()?;
        producer.connection.create_window(
            screen.root_depth,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new(),
        )?;
    }
    producer.connection.get_input_focus()?.reply()?;
    std::thread::sleep(Duration::from_millis(100));

    let started = std::time::Instant::now();
    handle.shutdown()?;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bounded observation worker became shutdown-unresponsive during flood"
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn observation_actor_owns_reconcile_health_and_shutdown_roundtrips()
-> Result<(), Box<dyn std::error::Error>> {
    let (handle, _events, join) = spawn_observation_actor(&display()?)?;
    let inventory = handle
        .try_reconcile()?
        .recv_timeout(Duration::from_secs(2))??;
    assert!(inventory.windows.len() <= xenoteer_x11::MAX_ROOT_WINDOWS);
    let health = handle
        .try_health_check()?
        .recv_timeout(Duration::from_secs(2))??;
    assert_eq!(health.state, ObservationActorState::Healthy);
    handle.shutdown().recv_timeout(Duration::from_secs(2))??;
    assert_eq!(join.join(), ObservationActorExit::Stopped);
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb with DAMAGE; run tests/platform/run-x11-spikes.sh"]
fn observation_actor_coalesces_live_root_damage() -> Result<(), Box<dyn std::error::Error>> {
    let (handle, events, join) = spawn_observation_actor(&display()?)?;
    let producer = connect(&display()?)?;
    producer.info.extensions.require(ExtensionName::Damage)?;
    let gc = producer.connection.generate_id()?;
    producer.connection.create_gc(
        gc,
        producer.info.root,
        &CreateGCAux::new().foreground(0x00a0_1020),
    )?;
    producer.connection.poly_fill_rectangle(
        producer.info.root,
        gc,
        &[Rectangle {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        }],
    )?;
    producer.connection.get_input_focus()?.reply()?;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ObservationActorEvent::RootDamaged { damage }) => {
                observed = Some(damage);
                break;
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("observation actor event lane closed before root damage".into());
            }
        }
    }
    let damage = observed.ok_or("observation actor did not emit coalesced root damage")?;
    assert!(!damage.regions.is_empty());
    assert!(damage.notifications >= 1);
    assert!(
        damage
            .regions
            .iter()
            .all(|region| region.width() > 0 && region.height() > 0)
    );

    handle.shutdown().recv_timeout(Duration::from_secs(2))??;
    assert_eq!(join.join(), ObservationActorExit::Stopped);
    producer.connection.free_gc(gc)?.check()?;
    Ok(())
}

#[test]
#[ignore = "requires authenticated Xvfb; use the XFCE profile for convergence"]
fn window_control_actor_owns_a_distinct_connection_and_returns_bounded_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let producer = connect(&display()?)?;
    let screen = &producer.connection.setup().roots[producer.info.screen_index];
    let window = producer.connection.generate_id()?;
    producer.connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        80,
        60,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new(),
    )?;
    producer.connection.map_window(window)?;
    producer.connection.get_input_focus()?.reply()?;

    let (handle, join) = spawn_window_control_actor(&display()?)?;
    let capabilities = handle
        .try_capabilities()?
        .recv_timeout(Duration::from_secs(2))??;
    assert!(capabilities.supported.len() <= 16);
    let requested = RawWindowControlRequest {
        target: window,
        operation: RawWindowControlOperation::Activate {
            timestamp: 0,
            switch_workspace: None,
            allow_set_input_focus: false,
        },
        timeout: Duration::from_millis(250),
    };
    let evidence = handle
        .try_submit(requested.clone(), || Ok(()))?
        .recv_timeout(Duration::from_secs(2))??;
    assert_eq!(evidence.requested, requested);
    assert!(
        matches!(
            evidence.outcome,
            RawWindowControlOutcome::Converged
                | RawWindowControlOutcome::Unsupported
                | RawWindowControlOutcome::Refused
                | RawWindowControlOutcome::TimedOut
                | RawWindowControlOutcome::Partial
        ),
        "unexpected window-control evidence: {evidence:?}"
    );
    assert_eq!(handle.health().state, WindowControlActorState::Healthy);
    handle.shutdown().recv_timeout(Duration::from_secs(2))??;
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
    producer.connection.destroy_window(window)?.check()?;
    Ok(())
}

#[test]
#[ignore = "requires the authenticated Phase-2 Xvfb/XFCE profile"]
fn frame_relative_clamp_uses_live_extents_and_waits_for_quiet_geometry()
-> Result<(), Box<dyn std::error::Error>> {
    let producer = connect(&display()?)?;
    let screen = &producer.connection.setup().roots[producer.info.screen_index];
    let window = producer.connection.generate_id()?;
    producer.connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        100,
        100,
        320,
        200,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )?;
    producer.connection.map_window(window)?;
    producer.connection.get_input_focus()?.reply()?;

    let frame_extents_atom = producer
        .connection
        .intern_atom(false, b"_NET_FRAME_EXTENTS")?
        .reply()?
        .atom;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = producer
            .connection
            .get_property(false, window, frame_extents_atom, AtomEnum::CARDINAL, 0, 4)?
            .reply()?;
        if reply.value32().is_some_and(|values| values.count() == 4) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("XFCE did not publish four live frame extents".into());
        }
        thread::sleep(Duration::from_millis(20));
    }

    let (handle, join) = spawn_window_control_actor(&display()?)?;
    let requested = RawWindowControlRequest {
        target: window,
        operation: RawWindowControlOperation::MoveResize {
            relative_to: WindowGeometryTarget::Frame,
            geometry: WindowGeometryRequest {
                x: Some(-200),
                y: Some(-100),
                width: Some(400),
                height: Some(300),
            },
            bounds_policy: WindowScreenBoundsPolicy::ClampToRoot,
        },
        timeout: Duration::from_secs(1),
    };
    let evidence = handle
        .try_submit(requested.clone(), || Ok(()))?
        .recv_timeout(Duration::from_secs(3))??;
    assert_eq!(evidence.requested, requested);
    assert_eq!(evidence.outcome, RawWindowControlOutcome::Converged);
    let xenoteer_x11::RawWindowControlObservation::Geometry(observed) = evidence.observed else {
        return Err("geometry control returned the wrong evidence family".into());
    };
    assert!(observed.bounds_constrained);
    assert!(observed.quiet);
    assert_eq!(observed.effective.rect.origin().x(), 0);
    assert_eq!(observed.effective.rect.origin().y(), 0);
    assert_eq!(observed.observed.frame_rect, Some(observed.effective));

    handle.shutdown().recv_timeout(Duration::from_secs(2))??;
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
    producer.connection.destroy_window(window)?.check()?;
    Ok(())
}

#[cfg(feature = "native-xkbcommon")]
#[test]
#[ignore = "requires libxkbcommon-x11 and authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn xkbcommon_reads_server_keymap() -> Result<(), Box<dyn std::error::Error>> {
    let model = xenoteer_x11::keyboard::NativeKeyboardModel::connect(&display()?)?;
    assert!(model.max_keycode() >= model.min_keycode());
    let mapping = model
        .first_symbol_mapping()
        .ok_or("server keymap contained no symbol mapping")?;
    assert!(mapping.keycode >= model.min_keycode());
    assert_ne!(mapping.keysym, 0);
    Ok(())
}

#[test]
#[ignore = "requires libxkbcommon-x11 and fresh authenticated Xvfb"]
fn xtest_keyboard_initialization_is_bounded_to_the_first_fake_input_request()
-> Result<(), Box<dyn std::error::Error>> {
    // Resolve from the core mapping before the observer subscribes. Avoiding
    // libxkbcommon entirely proves that no model connection can be the client
    // responsible for an event observed inside the XTEST bracket.
    let resolver = connect(&display()?)?;
    let keycode_count = resolver
        .info
        .max_keycode
        .checked_sub(resolver.info.min_keycode)
        .and_then(|count| count.checked_add(1))
        .ok_or("invalid server keycode range")?;
    let mapping = resolver
        .connection
        .get_keyboard_mapping(resolver.info.min_keycode, keycode_count)?
        .reply()?;
    let keysyms_per_keycode = usize::from(mapping.keysyms_per_keycode);
    if keysyms_per_keycode == 0 {
        return Err("server returned zero keysyms per keycode".into());
    }
    let keycode_for = |target: u32| {
        mapping
            .keysyms
            .chunks_exact(keysyms_per_keycode)
            .position(|keysyms| keysyms.contains(&target))
            .and_then(|offset| u8::try_from(offset).ok())
            .and_then(|offset| resolver.info.min_keycode.checked_add(offset))
    };
    let control_keycode = keycode_for(0xffe3).ok_or("core map lacks Control_L")?;
    let scalar_keycode = keycode_for(u32::from('v')).ok_or("core map lacks v")?;
    drop(resolver);

    let observer = connect(&display()?)?;
    observer.connection.xkb_use_extension(1, 0)?.reply()?;
    let new_keyboard = NKNDetail::KEYCODES | NKNDetail::DEVICE_ID;
    let details = SelectEventsAux::new().new_keyboard_notify(SelectEventsAuxNewKeyboardNotify {
        affect_new_keyboard: new_keyboard,
        new_keyboard_details: new_keyboard,
    });
    observer
        .connection
        .xkb_select_events(
            ID::USE_CORE_KBD.into(),
            EventType::default(),
            EventType::default(),
            MapPart::default(),
            MapPart::default(),
            &details,
        )?
        .check()?;
    observer.core_roundtrip()?;
    while observer.connection.poll_for_event()?.is_some() {}

    let sender = connect(&display()?)?;
    sender.connection.xtest_get_version(2, 2)?.reply()?;
    // Exclude sender connection setup and extension inventory from the
    // observation bracket as well.
    sender.core_roundtrip()?;
    observer.core_roundtrip()?;
    while observer.connection.poll_for_event()?.is_some() {}

    let mut notifications = Vec::new();
    for (step, event_type, keycode) in [
        ("control_press", KEY_PRESS_EVENT, control_keycode),
        ("scalar_press", KEY_PRESS_EVENT, scalar_keycode),
        ("scalar_release", KEY_RELEASE_EVENT, scalar_keycode),
        ("control_release", KEY_RELEASE_EVENT, control_keycode),
    ] {
        sender
            .connection
            .xtest_fake_input(event_type, keycode, 0, sender.info.root, 0, 0, 0)?
            .check()?;
        sender.core_roundtrip()?;
        observer.core_roundtrip()?;
        while let Some(event) = observer.connection.poll_for_event()? {
            if let x11rb::protocol::Event::XkbNewKeyboardNotify(event) = event {
                notifications.push((
                    step,
                    event.device_id,
                    event.old_device_id,
                    event.changed,
                    event.request_major,
                    event.request_minor,
                    event.time,
                    event.sequence,
                ));
            }
        }
    }
    assert!(notifications.len() <= 1, "{notifications:?}");
    if let Some((step, device, old_device, changed, _major, minor, _, _)) = notifications.first() {
        assert_eq!(*step, "control_press");
        assert_eq!(device, old_device);
        assert!(changed.contains(NKNDetail::KEYCODES));
        assert!(changed.contains(NKNDetail::GEOMETRY));
        assert_eq!(*minor, 9, "expected XkbSetMap initialization");
    }

    let mut repeated_notifications = Vec::new();
    for (step, event_type, keycode) in [
        ("control_press", KEY_PRESS_EVENT, control_keycode),
        ("scalar_press", KEY_PRESS_EVENT, scalar_keycode),
        ("scalar_release", KEY_RELEASE_EVENT, scalar_keycode),
        ("control_release", KEY_RELEASE_EVENT, control_keycode),
    ] {
        sender
            .connection
            .xtest_fake_input(event_type, keycode, 0, sender.info.root, 0, 0, 0)?
            .check()?;
        sender.core_roundtrip()?;
        observer.core_roundtrip()?;
        while let Some(event) = observer.connection.poll_for_event()? {
            if let x11rb::protocol::Event::XkbNewKeyboardNotify(event) = event {
                repeated_notifications.push((step, event.changed));
            }
        }
    }
    assert!(
        repeated_notifications.is_empty(),
        "XTEST keyboard initialization repeated after the first request: {repeated_notifications:?}"
    );
    Ok(())
}

#[cfg(feature = "native-xkbcommon")]
#[test]
#[ignore = "requires libxkbcommon-x11 and fresh authenticated Xvfb"]
fn input_actor_confirms_first_chord_after_xtest_keyboard_initialization()
-> Result<(), Box<dyn std::error::Error>> {
    let (handle, join) = spawn_input_actor(&display()?)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let reply = handle.try_submit_keyboard(
        ActionContext::new(CommandId::new(), None),
        KeyboardAction::chord(
            &[
                KeyIdentifier::Named(NamedKey::ControlLeft),
                KeyIdentifier::Scalar('v'),
            ],
            0,
        )?,
        CancellationToken::new(),
    )?;
    let outcome = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(3), reply).await })???;
    assert_eq!(outcome.kind, InputOutcomeKind::Completed);
    assert_eq!(outcome.events_emitted, 4);
    assert_eq!(outcome.completed_units, 1);
    assert!(
        handle
            .health()
            .keyboard_model
            .generation
            .is_some_and(|generation| generation >= 2)
    );
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), handle.shutdown()).await
    })???;
    assert_eq!(join.join(), InputActorExit::Stopped);
    Ok(())
}
