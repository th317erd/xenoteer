//! Independent XTEST driver with a same-connection QueryPointer barrier.

#![forbid(unsafe_code)]

use std::error::Error;

use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;

#[derive(Serialize)]
struct BarrierEvidence {
    r#type: &'static str,
    root_x: i16,
    root_y: i16,
    child: u32,
}

struct Options {
    x: i16,
    y: i16,
    expected_window: u32,
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let (connection, screen_index) = x11rb::connect(None)?;
    let root = connection
        .setup()
        .roots
        .get(screen_index)
        .ok_or("selected screen is absent")?
        .root;

    connection
        .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, root, options.x, options.y, 0)?
        .check()?;
    // QueryPointer is deliberately issued on this same connection. Its reply
    // is an ordering barrier for the preceding XTEST request.
    let reply = connection.query_pointer(root)?.reply()?;
    if (reply.root_x, reply.root_y) != (options.x, options.y) {
        return Err(format!(
            "QueryPointer endpoint mismatch: expected ({}, {}), got ({}, {})",
            options.x, options.y, reply.root_x, reply.root_y
        )
        .into());
    }
    if reply.child != options.expected_window {
        let attributes = connection
            .get_window_attributes(options.expected_window)?
            .reply()?;
        let geometry = connection.get_geometry(options.expected_window)?.reply()?;
        let tree = connection.query_tree(root)?.reply()?;
        return Err(format!(
            "pointer is over window {}, expected recorder window {}; expected map_state={}, geometry={}x{}+{}+{}, present_under_root={}",
            reply.child,
            options.expected_window,
            u8::from(attributes.map_state),
            geometry.width,
            geometry.height,
            geometry.x,
            geometry.y,
            tree.children.contains(&options.expected_window),
        )
        .into());
    }
    println!(
        "{}",
        serde_json::to_string(&BarrierEvidence {
            r#type: "query_pointer_barrier",
            root_x: reply.root_x,
            root_y: reply.root_y,
            child: reply.child,
        })?
    );
    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut x = None;
    let mut y = None;
    let mut expected_window = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--x" => x = Some(args.next().ok_or("--x requires an integer")?.parse()?),
            "--y" => y = Some(args.next().ok_or("--y requires an integer")?.parse()?),
            "--expected-window" => {
                expected_window = Some(
                    args.next()
                        .ok_or("--expected-window requires an integer")?
                        .parse()?,
                );
            }
            _ => {
                return Err("usage: x11-input-driver --x N --y N --expected-window WINDOW".into());
            }
        }
    }
    Ok(Options {
        x: x.ok_or("missing --x")?,
        y: y.ok_or("missing --y")?,
        expected_window: expected_window.ok_or("missing --expected-window")?,
    })
}
