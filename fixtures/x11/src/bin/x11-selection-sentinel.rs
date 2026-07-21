//! Own CLIPBOARD and PRIMARY, serve a canary, and fail if displaced.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;

use serde::Serialize;
use x11rb::CURRENT_TIME;
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

struct Arguments {
    canary_file: PathBuf,
}

fn parse_arguments() -> Result<Arguments, Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let program = arguments
        .next()
        .unwrap_or_else(|| "x11-selection-sentinel".into());
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--canary-file")) {
        return Err(format!(
            "usage: {} --canary-file PATH",
            PathBuf::from(program).display()
        )
        .into());
    }
    let canary_file = arguments
        .next()
        .ok_or("--canary-file requires a path")?
        .into();
    if arguments.next().is_some() {
        return Err("unexpected argument after canary file".into());
    }
    Ok(Arguments { canary_file })
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Evidence {
    Ready {
        window: u32,
        clipboard: u32,
        primary: u32,
        targets: u32,
        utf8_string: u32,
        text: u32,
        string: u32,
        canary_bytes: usize,
    },
    SelectionClear {
        selection: u32,
    },
    SelectionRequest {
        selection: u32,
        target: u32,
        requestor: u32,
        property: u32,
        response_property: u32,
        served_canary: bool,
    },
}

fn write_evidence(evidence: &Evidence) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, evidence)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = parse_arguments()?;
    let canary = fs::read(&arguments.canary_file)?;
    if canary.is_empty() || canary.len() > 4096 || canary.contains(&0) {
        return Err("canary must contain 1..=4096 non-NUL bytes".into());
    }

    let (connection, screen_index) = x11rb::connect(None)?;
    let root = connection
        .setup()
        .roots
        .get(screen_index)
        .ok_or("selected screen is absent")?
        .root;
    let window = connection.generate_id()?;
    connection.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let primary = connection.intern_atom(false, b"PRIMARY")?.reply()?.atom;
    let targets = connection.intern_atom(false, b"TARGETS")?.reply()?.atom;
    let utf8_string = connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let text = connection.intern_atom(false, b"TEXT")?.reply()?.atom;
    let string: u32 = AtomEnum::STRING.into();
    let atom: u32 = AtomEnum::ATOM.into();
    for selection in [clipboard, primary] {
        connection.set_selection_owner(window, selection, CURRENT_TIME)?;
    }
    connection.flush()?;
    for selection in [clipboard, primary] {
        let owner = connection.get_selection_owner(selection)?.reply()?.owner;
        if owner != window {
            return Err(format!(
                "selection {selection} owner was {owner}, expected sentinel {window}"
            )
            .into());
        }
    }
    write_evidence(&Evidence::Ready {
        window,
        clipboard,
        primary,
        targets,
        utf8_string,
        text,
        string,
        canary_bytes: canary.len(),
    })?;

    loop {
        match connection.wait_for_event()? {
            Event::SelectionClear(event) => {
                write_evidence(&Evidence::SelectionClear {
                    selection: event.selection,
                })?;
                return Err(
                    format!("selection {} ownership was displaced", event.selection).into(),
                );
            }
            Event::SelectionRequest(event) => {
                let response_property = if event.property == NONE {
                    event.target
                } else {
                    event.property
                };
                let served_canary = if event.target == targets {
                    connection.change_property32(
                        PropMode::REPLACE,
                        event.requestor,
                        response_property,
                        atom,
                        &[targets, utf8_string, text, string],
                    )?;
                    false
                } else if [utf8_string, text, string].contains(&event.target) {
                    connection.change_property8(
                        PropMode::REPLACE,
                        event.requestor,
                        response_property,
                        event.target,
                        &canary,
                    )?;
                    true
                } else {
                    false
                };
                let notify_property = if event.target == targets || served_canary {
                    response_property
                } else {
                    NONE
                };
                connection.send_event(
                    false,
                    event.requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: event.sequence,
                        time: event.time,
                        requestor: event.requestor,
                        selection: event.selection,
                        target: event.target,
                        property: notify_property,
                    },
                )?;
                connection.flush()?;
                write_evidence(&Evidence::SelectionRequest {
                    selection: event.selection,
                    target: event.target,
                    requestor: event.requestor,
                    property: event.property,
                    response_property: notify_property,
                    served_canary,
                })?;
            }
            _ => {}
        }
    }
}
