//! Bounded top-level X11 lifecycle churn for observation-loss acceptance tests.

#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{self, Write as _};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

const DEFAULT_ITERATIONS: u32 = 16_384;
const DEFAULT_BATCH_SIZE: u32 = 256;
const DEFAULT_BATCH_PAUSE_MS: u64 = 5;
const DEFAULT_START_DELAY_MS: u64 = 3_000;
const DEFAULT_HOLD_AFTER_MS: u64 = 8_000;
const MAX_ITERATIONS: u32 = 100_000;
const MAX_BATCH_SIZE: u32 = 4_096;
const MAX_PAUSE_MS: u64 = 10_000;

#[derive(Serialize)]
struct Ready {
    r#type: &'static str,
    sentinel_window: u32,
    iterations: u32,
    batch_size: u32,
}

#[derive(Clone, Copy)]
struct Options {
    iterations: u32,
    batch_size: u32,
    batch_pause_ms: u64,
    start_delay_ms: u64,
    hold_after_ms: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let (connection, screen_index) = x11rb::connect(None)?;
    let screen = &connection.setup().roots[screen_index];
    let sentinel = connection.generate_id()?;
    connection.create_window(
        screen.root_depth,
        sentinel,
        screen.root,
        40,
        40,
        320,
        120,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        sentinel,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Xenoteer Event Flood Sentinel",
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        sentinel,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"xenoteer-event-flood\0XenoteerEventFlood\0",
    )?;
    connection.map_window(sentinel)?;
    barrier(&connection)?;
    write_ready(sentinel, options)?;
    if options.start_delay_ms > 0 {
        thread::sleep(Duration::from_millis(options.start_delay_ms));
    }
    write_churn_started()?;

    for index in 0..options.iterations {
        let window = connection.generate_id()?;
        connection.create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        connection.destroy_window(window)?;

        let completed = index + 1;
        if completed % options.batch_size == 0 || completed == options.iterations {
            barrier(&connection)?;
            if completed != options.iterations && options.batch_pause_ms > 0 {
                thread::sleep(Duration::from_millis(options.batch_pause_ms));
            }
        }
    }

    if options.hold_after_ms > 0 {
        thread::sleep(Duration::from_millis(options.hold_after_ms));
    }
    connection.destroy_window(sentinel)?;
    barrier(&connection)?;
    Ok(())
}

fn barrier<C: Connection>(connection: &C) -> Result<(), Box<dyn Error>> {
    connection.flush()?;
    connection.get_input_focus()?.reply()?;
    Ok(())
}

fn write_ready(window: u32, options: Options) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(
        &mut output,
        &Ready {
            r#type: "ready",
            sentinel_window: window,
            iterations: options.iterations,
            batch_size: options.batch_size,
        },
    )?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn write_churn_started() -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(b"{\"type\":\"churn_started\"}\n")?;
    output.flush()?;
    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        iterations: DEFAULT_ITERATIONS,
        batch_size: DEFAULT_BATCH_SIZE,
        batch_pause_ms: DEFAULT_BATCH_PAUSE_MS,
        start_delay_ms: DEFAULT_START_DELAY_MS,
        hold_after_ms: DEFAULT_HOLD_AFTER_MS,
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value after {argument}"))?;
        match argument.as_str() {
            "--iterations" => {
                options.iterations = bounded_u32(&value, 1, MAX_ITERATIONS, "iterations")?;
            }
            "--batch-size" => {
                options.batch_size = bounded_u32(&value, 1, MAX_BATCH_SIZE, "batch size")?;
            }
            "--batch-pause-ms" => {
                options.batch_pause_ms = bounded_u64(&value, MAX_PAUSE_MS, "batch pause")?;
            }
            "--start-delay-ms" => {
                options.start_delay_ms = bounded_u64(&value, MAX_PAUSE_MS, "start delay")?;
            }
            "--hold-after-ms" => {
                options.hold_after_ms = bounded_u64(&value, MAX_PAUSE_MS, "post-churn hold")?;
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    if options.batch_size > options.iterations {
        return Err("batch size cannot exceed iterations".into());
    }
    Ok(options)
}

fn bounded_u32(value: &str, minimum: u32, maximum: u32, name: &str) -> Result<u32, Box<dyn Error>> {
    let parsed: u32 = value.parse()?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(format!("{name} must be between {minimum} and {maximum}").into());
    }
    Ok(parsed)
}

fn bounded_u64(value: &str, maximum: u64, name: &str) -> Result<u64, Box<dyn Error>> {
    let parsed: u64 = value.parse()?;
    if parsed > maximum {
        return Err(format!("{name} must be at most {maximum}").into());
    }
    Ok(parsed)
}
