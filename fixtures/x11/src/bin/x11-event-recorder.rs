//! Standalone JSONL X11 input/event recorder used as independent effect proof.

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
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordedEvent {
    Ready {
        window: u32,
    },
    Motion {
        window: u32,
        time: u32,
        root_x: i16,
        root_y: i16,
        event_x: i16,
        event_y: i16,
        state: u16,
    },
    ButtonPress {
        window: u32,
        time: u32,
        detail: u8,
        state: u16,
    },
    ButtonRelease {
        window: u32,
        time: u32,
        detail: u8,
        state: u16,
    },
    KeyPress {
        window: u32,
        time: u32,
        keycode: u8,
        keysym: u32,
        state: u16,
    },
    KeyRelease {
        window: u32,
        time: u32,
        keycode: u8,
        keysym: u32,
        state: u16,
    },
    FocusIn {
        window: u32,
        mode: u8,
        detail: u8,
    },
    FocusOut {
        window: u32,
        mode: u8,
        detail: u8,
    },
    Enter {
        window: u32,
        time: u32,
        root_x: i16,
        root_y: i16,
        state: u16,
    },
    Leave {
        window: u32,
        time: u32,
        root_x: i16,
        root_y: i16,
        state: u16,
    },
    Configure {
        window: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
    Map {
        window: u32,
    },
    Unmap {
        window: u32,
    },
    Destroy {
        window: u32,
    },
}

struct Options {
    max_events: Option<u64>,
    exit_after_motion: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let (connection, screen_index) = x11rb::connect(None)?;
    let screen = &connection.setup().roots[screen_index];
    let window = connection.generate_id()?;
    connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        640,
        480,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::POINTER_MOTION
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::FOCUS_CHANGE
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW,
            ),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Xenoteer X11 Event Recorder",
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"xenoteer-event-recorder\0XenoteerEventRecorder\0",
    )?;
    let gc = connection.generate_id()?;
    connection.create_gc(
        gc,
        window,
        &CreateGCAux::new().foreground(screen.black_pixel),
    )?;
    connection.map_window(window)?;
    connection.flush()?;

    let mut recorded = 0_u64;
    let mut ready = false;
    loop {
        let event = connection.wait_for_event()?;
        let normalized = match event {
            Event::Expose(_) => {
                paint_grid(&connection, window, gc, screen.black_pixel)?;
                if !ready {
                    // A reply-producing request on the same connection proves the
                    // server processed all preceding paint requests before Ready.
                    connection.get_input_focus()?.reply()?;
                    write_event(&RecordedEvent::Ready { window })?;
                    ready = true;
                }
                None
            }
            Event::MotionNotify(event) => Some(RecordedEvent::Motion {
                window: event.event,
                time: event.time,
                root_x: event.root_x,
                root_y: event.root_y,
                event_x: event.event_x,
                event_y: event.event_y,
                state: event.state.into(),
            }),
            Event::ButtonPress(event) => Some(RecordedEvent::ButtonPress {
                window: event.event,
                time: event.time,
                detail: event.detail,
                state: event.state.into(),
            }),
            Event::ButtonRelease(event) => Some(RecordedEvent::ButtonRelease {
                window: event.event,
                time: event.time,
                detail: event.detail,
                state: event.state.into(),
            }),
            Event::KeyPress(event) => Some(RecordedEvent::KeyPress {
                window: event.event,
                time: event.time,
                keycode: event.detail,
                keysym: resolve_keysym(&connection, event.detail)?,
                state: event.state.into(),
            }),
            Event::KeyRelease(event) => Some(RecordedEvent::KeyRelease {
                window: event.event,
                time: event.time,
                keycode: event.detail,
                keysym: resolve_keysym(&connection, event.detail)?,
                state: event.state.into(),
            }),
            Event::FocusIn(event) => Some(RecordedEvent::FocusIn {
                window: event.event,
                mode: event.mode.into(),
                detail: event.detail.into(),
            }),
            Event::FocusOut(event) => Some(RecordedEvent::FocusOut {
                window: event.event,
                mode: event.mode.into(),
                detail: event.detail.into(),
            }),
            Event::EnterNotify(event) => Some(RecordedEvent::Enter {
                window: event.event,
                time: event.time,
                root_x: event.root_x,
                root_y: event.root_y,
                state: event.state.into(),
            }),
            Event::LeaveNotify(event) => Some(RecordedEvent::Leave {
                window: event.event,
                time: event.time,
                root_x: event.root_x,
                root_y: event.root_y,
                state: event.state.into(),
            }),
            Event::ConfigureNotify(event) => Some(RecordedEvent::Configure {
                window: event.window,
                x: event.x,
                y: event.y,
                width: event.width,
                height: event.height,
            }),
            Event::MapNotify(event) => Some(RecordedEvent::Map {
                window: event.window,
            }),
            Event::UnmapNotify(event) => Some(RecordedEvent::Unmap {
                window: event.window,
            }),
            Event::DestroyNotify(event) => Some(RecordedEvent::Destroy {
                window: event.window,
            }),
            _ => None,
        };
        if !ready {
            continue;
        }
        if let Some(normalized) = normalized {
            let is_motion = matches!(normalized, RecordedEvent::Motion { .. });
            write_event(&normalized)?;
            recorded = recorded.saturating_add(1);
            if (options.exit_after_motion && is_motion)
                || options.max_events.is_some_and(|limit| recorded >= limit)
            {
                return Ok(());
            }
        }
    }
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        max_events: None,
        exit_after_motion: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--max-events" => {
                let value = args.next().ok_or("--max-events requires an integer")?;
                options.max_events = Some(value.parse()?);
            }
            "--exit-after-motion" => options.exit_after_motion = true,
            _ => {
                return Err(
                    "usage: x11-event-recorder [--max-events N] [--exit-after-motion]".into(),
                );
            }
        }
    }
    Ok(options)
}

fn resolve_keysym<C: Connection>(connection: &C, keycode: u8) -> Result<u32, Box<dyn Error>> {
    let reply = connection.get_keyboard_mapping(keycode, 1)?.reply()?;
    Ok(reply.keysyms.first().copied().unwrap_or(0))
}

fn paint_grid<C: Connection>(
    connection: &C,
    window: u32,
    gc: u32,
    foreground: u32,
) -> Result<(), Box<dyn Error>> {
    connection.change_gc(gc, &ChangeGCAux::new().foreground(foreground))?;
    let mut rectangles = Vec::new();
    for x in (0_i16..640).step_by(40) {
        rectangles.push(Rectangle {
            x,
            y: 0,
            width: 1,
            height: 480,
        });
    }
    for y in (0_i16..480).step_by(40) {
        rectangles.push(Rectangle {
            x: 0,
            y,
            width: 640,
            height: 1,
        });
    }
    connection.poly_fill_rectangle(window, gc, &rectangles)?;
    connection.flush()?;
    Ok(())
}

fn write_event(event: &RecordedEvent) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, event)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
