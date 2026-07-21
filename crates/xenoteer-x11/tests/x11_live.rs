//! Real Xvfb/XTEST/xkbcommon backend proofs.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle,
    WindowClass,
};
use xenoteer_x11::{
    ExtensionName, OBSERVATION_EVENT_CAPACITY, ObservationPollThread, PollThreadEvent, connect,
    fake_absolute_motion, query_pointer_barrier,
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
