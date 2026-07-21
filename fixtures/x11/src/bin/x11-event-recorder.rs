//! Standalone JSONL X11 input/event recorder used as independent effect proof.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::io::{self, Write as _};

use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, InputFocus, KeyButMask, Mapping, PropMode, Rectangle, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

const WINDOW_WIDTH: u16 = 640;
const WINDOW_HEIGHT: u16 = 480;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordedEvent {
    Ready {
        window: u32,
    },
    ReadyMetadata {
        window: u32,
        painted: bool,
        focus_requested: bool,
        observed_focus: u32,
        pointer_grab_requested: bool,
        pointer_grabbed: bool,
        max_events: Option<u64>,
        post_motion_warp: Option<RootPoint>,
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
        root_x: i16,
        root_y: i16,
        event_x: i16,
        event_y: i16,
        state: u16,
    },
    ButtonRelease {
        window: u32,
        time: u32,
        detail: u8,
        root_x: i16,
        root_y: i16,
        event_x: i16,
        event_y: i16,
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
    PointerWarped {
        requested_root_x: i16,
        requested_root_y: i16,
        observed_root_x: i16,
        observed_root_y: i16,
    },
    PointerUngrabbed {
        reason: &'static str,
    },
    DestroyRequested {
        window: u32,
    },
    MappingNotify {
        request: u8,
        first_keycode: u8,
        count: u8,
    },
}

#[derive(Clone, Copy, Serialize)]
struct RootPoint {
    x: i16,
    y: i16,
}

struct Options {
    max_events: Option<u64>,
    exit_after_motion: bool,
    focus_before_ready: bool,
    post_motion_warp: Option<RootPoint>,
    grab_pointer: bool,
    release_grab_after_button_press: bool,
    destroy_after_button_press: bool,
}

#[derive(Default)]
struct PaintedInputState {
    pointer: Option<(i16, i16)>,
    pressed_buttons: BTreeSet<u8>,
    pressed_keys: BTreeSet<u8>,
}

struct CoreKeymap {
    first_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: Vec<u32>,
}

impl CoreKeymap {
    fn fetch<C: Connection>(connection: &C) -> Result<Self, Box<dyn Error>> {
        let first_keycode = connection.setup().min_keycode;
        let last_keycode = connection.setup().max_keycode;
        let count = last_keycode
            .checked_sub(first_keycode)
            .and_then(|difference| difference.checked_add(1))
            .ok_or("X server advertised an invalid core keycode range")?;
        let reply = connection
            .get_keyboard_mapping(first_keycode, count)?
            .reply()?;
        Ok(Self {
            first_keycode,
            keysyms_per_keycode: usize::from(reply.keysyms_per_keycode),
            keysyms: reply.keysyms,
        })
    }

    fn first_keysym(&self, keycode: u8) -> u32 {
        let Some(offset) = keycode.checked_sub(self.first_keycode) else {
            return 0;
        };
        let index = usize::from(offset).saturating_mul(self.keysyms_per_keycode);
        self.keysyms.get(index).copied().unwrap_or(0)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let (connection, screen_index) = x11rb::connect(None)?;
    // Some Xvfb/XKB combinations lazily compile their core compatibility map
    // on the first GetKeyboardMapping request and broadcast MappingNotify as a
    // result. Initialize and converge that read-only cache before the fixture
    // creates a window, so those server-initialization broadcasts cannot be
    // mistaken for a mapping change caused by the actor's first key event.
    let mut core_keymap = initialize_core_keymap(&connection)?;
    let screen = &connection.setup().roots[screen_index];
    let window = connection.generate_id()?;
    connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
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
    let mut pointer_grabbed = false;
    let mut destroy_requested = false;
    let mut post_motion_warp = options.post_motion_warp;
    let mut state = PaintedInputState::default();
    loop {
        let event = connection.wait_for_event()?;
        let normalized = match event {
            Event::Expose(_) => {
                if !ready {
                    initialize_input_state(&connection, window, &mut state)?;
                    paint_state(
                        &connection,
                        window,
                        gc,
                        screen.black_pixel,
                        screen.white_pixel,
                        &state,
                    )?;
                    if options.focus_before_ready {
                        connection
                            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
                            .check()?;
                    }
                    if options.grab_pointer {
                        let reply = connection
                            .grab_pointer(
                                false,
                                window,
                                EventMask::POINTER_MOTION
                                    | EventMask::BUTTON_PRESS
                                    | EventMask::BUTTON_RELEASE
                                    | EventMask::ENTER_WINDOW
                                    | EventMask::LEAVE_WINDOW,
                                GrabMode::ASYNC,
                                GrabMode::ASYNC,
                                NONE,
                                NONE,
                                CURRENT_TIME,
                            )?
                            .reply()?;
                        if reply.status != GrabStatus::SUCCESS {
                            return Err(format!(
                                "pointer grab failed with status {}",
                                u8::from(reply.status)
                            )
                            .into());
                        }
                        pointer_grabbed = true;
                    }
                    // A reply-producing request on the same connection proves the
                    // server processed all preceding paint requests before Ready.
                    let focus = connection.get_input_focus()?.reply()?.focus;
                    write_event(&RecordedEvent::Ready { window })?;
                    write_event(&RecordedEvent::ReadyMetadata {
                        window,
                        painted: true,
                        focus_requested: options.focus_before_ready,
                        observed_focus: focus,
                        pointer_grab_requested: options.grab_pointer,
                        pointer_grabbed,
                        max_events: options.max_events,
                        post_motion_warp: options.post_motion_warp,
                    })?;
                    ready = true;
                }
                None
            }
            Event::MotionNotify(event) => {
                state.pointer = Some((event.event_x, event.event_y));
                Some(RecordedEvent::Motion {
                    window: event.event,
                    time: event.time,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state.into(),
                })
            }
            Event::ButtonPress(event) => {
                state.pressed_buttons.insert(event.detail);
                Some(RecordedEvent::ButtonPress {
                    window: event.event,
                    time: event.time,
                    detail: event.detail,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state.into(),
                })
            }
            Event::ButtonRelease(event) => {
                state.pressed_buttons.remove(&event.detail);
                Some(RecordedEvent::ButtonRelease {
                    window: event.event,
                    time: event.time,
                    detail: event.detail,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state.into(),
                })
            }
            Event::KeyPress(event) => {
                state.pressed_keys.insert(event.detail);
                Some(RecordedEvent::KeyPress {
                    window: event.event,
                    time: event.time,
                    keycode: event.detail,
                    keysym: core_keymap.first_keysym(event.detail),
                    state: event.state.into(),
                })
            }
            Event::KeyRelease(event) => {
                state.pressed_keys.remove(&event.detail);
                Some(RecordedEvent::KeyRelease {
                    window: event.event,
                    time: event.time,
                    keycode: event.detail,
                    keysym: core_keymap.first_keysym(event.detail),
                    state: event.state.into(),
                })
            }
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
            Event::MappingNotify(event) => {
                if event.request == Mapping::KEYBOARD {
                    core_keymap = CoreKeymap::fetch(&connection)?;
                }
                Some(RecordedEvent::MappingNotify {
                    request: event.request.into(),
                    first_keycode: event.first_keycode,
                    count: event.count,
                })
            }
            _ => None,
        };
        if !ready {
            continue;
        }
        if let Some(normalized) = normalized {
            let is_motion = matches!(normalized, RecordedEvent::Motion { .. });
            let is_button_press = matches!(normalized, RecordedEvent::ButtonPress { .. });
            let is_own_destroy =
                matches!(normalized, RecordedEvent::Destroy { window: id } if id == window);
            let changes_painted_state = matches!(
                normalized,
                RecordedEvent::Motion { .. }
                    | RecordedEvent::ButtonPress { .. }
                    | RecordedEvent::ButtonRelease { .. }
                    | RecordedEvent::KeyPress { .. }
                    | RecordedEvent::KeyRelease { .. }
            );
            if changes_painted_state && !destroy_requested {
                paint_state(
                    &connection,
                    window,
                    gc,
                    screen.black_pixel,
                    screen.white_pixel,
                    &state,
                )?;
            }
            write_event(&normalized)?;
            recorded = recorded.saturating_add(1);

            if is_motion && let Some(point) = post_motion_warp.take() {
                connection
                    .warp_pointer(NONE, screen.root, 0, 0, 0, 0, point.x, point.y)?
                    .check()?;
                let observed = connection.query_pointer(screen.root)?.reply()?;
                write_event(&RecordedEvent::PointerWarped {
                    requested_root_x: point.x,
                    requested_root_y: point.y,
                    observed_root_x: observed.root_x,
                    observed_root_y: observed.root_y,
                })?;
            }

            if is_button_press {
                if options.release_grab_after_button_press && pointer_grabbed {
                    release_pointer_grab(&connection, "button_press")?;
                    pointer_grabbed = false;
                }
                if options.destroy_after_button_press && !destroy_requested {
                    connection.destroy_window(window)?.check()?;
                    write_event(&RecordedEvent::DestroyRequested { window })?;
                    destroy_requested = true;
                }
            }

            if is_own_destroy {
                return Ok(());
            }
            if (options.exit_after_motion && is_motion)
                || options.max_events.is_some_and(|limit| recorded >= limit)
            {
                if pointer_grabbed {
                    release_pointer_grab(&connection, "event_limit")?;
                }
                return Ok(());
            }
        }
    }
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        max_events: None,
        exit_after_motion: false,
        focus_before_ready: false,
        post_motion_warp: None,
        grab_pointer: false,
        release_grab_after_button_press: false,
        destroy_after_button_press: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--max-events" => {
                let value = args.next().ok_or("--max-events requires an integer")?;
                options.max_events = Some(value.parse()?);
            }
            "--exit-after-motion" => options.exit_after_motion = true,
            "--focus-before-ready" => options.focus_before_ready = true,
            "--post-motion-warp" => {
                let x = args
                    .next()
                    .ok_or("--post-motion-warp requires X and Y")?
                    .parse()?;
                let y = args
                    .next()
                    .ok_or("--post-motion-warp requires X and Y")?
                    .parse()?;
                options.post_motion_warp = Some(RootPoint { x, y });
            }
            "--grab-pointer" => options.grab_pointer = true,
            "--release-pointer-grab-after-button-press" => {
                options.release_grab_after_button_press = true;
            }
            "--destroy-after-button-press" => options.destroy_after_button_press = true,
            _ => {
                return Err(usage().into());
            }
        }
    }
    if options.release_grab_after_button_press && !options.grab_pointer {
        return Err("--release-pointer-grab-after-button-press requires --grab-pointer".into());
    }
    Ok(options)
}

fn usage() -> &'static str {
    "usage: x11-event-recorder [--max-events N] [--exit-after-motion] \
     [--focus-before-ready] [--post-motion-warp X Y] [--grab-pointer] \
     [--release-pointer-grab-after-button-press] [--destroy-after-button-press]"
}

fn initialize_core_keymap<C: Connection>(connection: &C) -> Result<CoreKeymap, Box<dyn Error>> {
    const MAX_CONVERGENCE_ROUNDS: u8 = 8;
    const REQUIRED_CLEAN_ROUNDS: u8 = 2;

    let mut keymap = CoreKeymap::fetch(connection)?;
    let mut consecutive_clean = 0_u8;
    for _round in 0..MAX_CONVERGENCE_ROUNDS {
        // This reply orders all server work caused by the preceding complete
        // map read before the event drain below.
        connection.get_input_focus()?.reply()?;
        let mut keyboard_mapping_invalidated = false;
        while let Some(event) = connection.poll_for_event()? {
            match event {
                Event::MappingNotify(event) => {
                    if event.request == Mapping::KEYBOARD {
                        keyboard_mapping_invalidated = true;
                    }
                }
                _ => {
                    return Err(
                        "unexpected X event while converging the pre-window core keymap".into(),
                    );
                }
            }
        }
        if keyboard_mapping_invalidated {
            keymap = CoreKeymap::fetch(connection)?;
            consecutive_clean = 0;
        } else {
            consecutive_clean = consecutive_clean.saturating_add(1);
            if consecutive_clean == REQUIRED_CLEAN_ROUNDS {
                return Ok(keymap);
            }
        }
    }
    Err(
        format!("core keymap did not converge within {MAX_CONVERGENCE_ROUNDS} synchronized rounds")
            .into(),
    )
}

fn initialize_input_state<C: Connection>(
    connection: &C,
    window: u32,
    state: &mut PaintedInputState,
) -> Result<(), Box<dyn Error>> {
    let pointer = connection.query_pointer(window)?.reply()?;
    state.pointer = Some((pointer.win_x, pointer.win_y));
    state.pressed_buttons.clear();
    for (button, mask) in [
        (1, KeyButMask::BUTTON1),
        (2, KeyButMask::BUTTON2),
        (3, KeyButMask::BUTTON3),
        (4, KeyButMask::BUTTON4),
        (5, KeyButMask::BUTTON5),
    ] {
        if pointer.mask.contains(mask) {
            state.pressed_buttons.insert(button);
        }
    }

    state.pressed_keys.clear();
    let keymap = connection.query_keymap()?.reply()?;
    for (byte_index, byte) in keymap.keys.iter().copied().enumerate() {
        for bit in 0_u8..8 {
            if byte & (1_u8 << bit) != 0 {
                let keycode = u8::try_from(byte_index * 8 + usize::from(bit))?;
                state.pressed_keys.insert(keycode);
            }
        }
    }
    Ok(())
}

fn paint_state<C: Connection>(
    connection: &C,
    window: u32,
    gc: u32,
    foreground: u32,
    background: u32,
    state: &PaintedInputState,
) -> Result<(), Box<dyn Error>> {
    connection.change_gc(gc, &ChangeGCAux::new().foreground(background))?;
    connection.poly_fill_rectangle(
        window,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
        }],
    )?;
    connection.change_gc(gc, &ChangeGCAux::new().foreground(foreground))?;
    let mut rectangles = Vec::new();
    for x in (0_i16..i16::try_from(WINDOW_WIDTH)?).step_by(40) {
        rectangles.push(Rectangle {
            x,
            y: 0,
            width: 1,
            height: WINDOW_HEIGHT,
        });
    }
    for y in (0_i16..i16::try_from(WINDOW_HEIGHT)?).step_by(40) {
        rectangles.push(Rectangle {
            x: 0,
            y,
            width: WINDOW_WIDTH,
            height: 1,
        });
    }
    if let Some((x, y)) = state.pointer {
        rectangles.push(Rectangle {
            x: x.clamp(0, i16::try_from(WINDOW_WIDTH)? - 9),
            y: y.clamp(0, i16::try_from(WINDOW_HEIGHT)? - 9),
            width: 9,
            height: 9,
        });
    }
    for button in &state.pressed_buttons {
        rectangles.push(Rectangle {
            x: 4 + i16::from(*button).saturating_mul(12),
            y: 4,
            width: 8,
            height: 20,
        });
    }
    for keycode in &state.pressed_keys {
        let column = i16::from(*keycode % 64);
        let row = i16::from(*keycode / 64);
        rectangles.push(Rectangle {
            x: 64 + column * 8,
            y: 440 + row * 8,
            width: 6,
            height: 6,
        });
    }
    connection.poly_fill_rectangle(window, gc, &rectangles)?;
    connection.flush()?;
    // A consumer may screenshot as soon as it reads the corresponding JSONL
    // input record. Make that record a server-observed paint boundary rather
    // than merely proof that bytes reached the client socket.
    connection.get_input_focus()?.reply()?;
    Ok(())
}

fn release_pointer_grab<C: Connection>(
    connection: &C,
    reason: &'static str,
) -> Result<(), Box<dyn Error>> {
    connection.ungrab_pointer(CURRENT_TIME)?.check()?;
    connection.get_input_focus()?.reply()?;
    write_event(&RecordedEvent::PointerUngrabbed { reason })?;
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
