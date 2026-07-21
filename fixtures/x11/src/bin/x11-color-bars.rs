//! Deterministic X11 color-bar window for GetImage/capture tests.

#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{self, Write as _};

use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode,
    Rectangle, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

#[derive(Serialize)]
struct Ready {
    r#type: &'static str,
    window: u32,
    width: u16,
    height: u16,
    bars_bgr: [&'static str; 5],
}

fn main() -> Result<(), Box<dyn Error>> {
    let exit_after_expose = std::env::args()
        .skip(1)
        .any(|arg| arg == "--exit-after-expose");
    let (connection, screen_index) = x11rb::connect(None)?;
    let screen = &connection.setup().roots[screen_index];
    let window = connection.generate_id()?;
    connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        20,
        20,
        500,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Xenoteer Color Bars",
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"xenoteer-color-bars\0XenoteerColorBars\0",
    )?;
    let gc = connection.generate_id()?;
    connection.create_gc(gc, window, &CreateGCAux::new())?;
    connection.map_window(window)?;
    connection.flush()?;

    let mut ready = false;
    loop {
        match connection.wait_for_event()? {
            Event::Expose(_) => {
                paint(&connection, window, gc)?;
                if !ready {
                    // Ready is a capture boundary: the same connection observes a
                    // reply only after the server has processed the paint requests.
                    connection.get_input_focus()?.reply()?;
                    write_ready(window)?;
                    ready = true;
                }
                if exit_after_expose {
                    return Ok(());
                }
            }
            Event::DestroyNotify(_) => return Ok(()),
            _ => {}
        }
    }
}

fn write_ready(window: u32) -> Result<(), Box<dyn Error>> {
    let ready = Ready {
        r#type: "ready",
        window,
        width: 500,
        height: 100,
        bars_bgr: ["red", "green", "blue", "white", "black"],
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &ready)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn paint<C: Connection>(connection: &C, window: u32, gc: u32) -> Result<(), Box<dyn Error>> {
    let colors = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff, 0];
    for (index, color) in colors.into_iter().enumerate() {
        connection.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        connection.poly_fill_rectangle(
            window,
            gc,
            &[Rectangle {
                x: i16::try_from(index * 100)?,
                y: 0,
                width: 100,
                height: 100,
            }],
        )?;
    }
    connection.flush()?;
    Ok(())
}
