//! Live, JSONL-emitting Phase 1 X11 input conformance driver.

#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{self, Write as _};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use xenoteer_core::domain::RootPoint;
use xenoteer_core::input::{
    ClickAction, DEFAULT_DOUBLE_CLICK_THRESHOLD_MS, DragAction, InputAction, LogicalButton,
    MotionCurve, MotionOptions, MotionPolicy, MoveAction, ScrollAction, ScrollDirection,
    WaypointDurationPolicy, plan_motion, plan_waypoint_motion,
};
use xenoteer_protocol::CommandId;
use xenoteer_x11::input::{
    ActionContext, InputActorExit, InputActorHandle, InputEffectEvidence, InputFailure,
    InputOutcome, InputOutcomeKind, PhysicalTextMode, spawn_input_actor,
};
#[cfg(feature = "native-xkbcommon")]
use xenoteer_x11::input::{KeyboardAction, KeyboardSequenceStep};
#[cfg(feature = "native-xkbcommon")]
use xenoteer_x11::keyboard::{KeyIdentifier, NamedKey};

const SCENARIO: &str = "conformance";

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record<'a> {
    Started {
        scenario: &'a str,
        window: u32,
        display: &'a str,
    },
    Action {
        name: &'a str,
        result: &'a str,
        events_emitted: usize,
        completed_units: u16,
        requested_pointer: Option<PointRecord>,
        observed_pointer: Option<PointRecord>,
        keyboard_bindings: Option<usize>,
        text_scalar_count: Option<usize>,
        requested_text_mode: Option<&'a str>,
        current_layout_scalars: Option<usize>,
        temporary_mapping_scalars: Option<usize>,
        temporary_mappings_installed: Option<usize>,
        temporary_mappings_restored: Option<usize>,
        temporary_mapping_restoration_proven: Option<bool>,
        effect_evidence: &'a str,
        effect_provisional: Option<usize>,
        effect_confirmed: Option<usize>,
    },
    IndependentObservation {
        elapsed_ms: u128,
        pointer: PointRecord,
        while_action_pending: bool,
    },
    CancellationBoundary {
        result: &'a str,
        events_emitted: usize,
        completed_units: u16,
        observed_pointer: Option<PointRecord>,
        cleanup_succeeded: Option<bool>,
    },
    #[cfg(feature = "native-xkbcommon")]
    KeyboardPrime {
        keycode: u8,
        result: &'a str,
        events_emitted: usize,
        cleanup_succeeded: Option<bool>,
    },
    #[cfg(feature = "native-xkbcommon")]
    TemporaryMappingProof {
        keycode: u8,
        keysyms_per_keycode: u8,
        mapping_word_count: usize,
        before_all_no_symbol: bool,
        before_unpressed: bool,
        before_nonmodifier: bool,
        after_exact_match: bool,
        after_unpressed: bool,
        after_nonmodifier: bool,
    },
    Complete {
        scenario: &'a str,
        window: u32,
        actor_exit: &'a str,
        pointer_actions: usize,
        keyboard_actions: usize,
    },
}

#[derive(Clone, Copy, Serialize)]
struct PointRecord {
    x: i32,
    y: i32,
}

impl From<RootPoint> for PointRecord {
    fn from(point: RootPoint) -> Self {
        Self {
            x: point.x(),
            y: point.y(),
        }
    }
}

struct Options {
    window: u32,
    scenario: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    if options.scenario != SCENARIO {
        return Err(format!("unsupported scenario: {}", options.scenario).into());
    }
    let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is not set")?;
    let (observer, screen_index) = x11rb::connect(Some(&display))?;
    observer.get_geometry(options.window)?.reply()?;
    let root = observer.setup().roots[screen_index].root;

    write_record(&Record::Started {
        scenario: SCENARIO,
        window: options.window,
        display: &display,
    })?;

    let (handle, join) = spawn_input_actor(&display)?;
    let mut action_count = 0_usize;

    prime_keyboard_for_live_conformance(&handle, &observer)?;

    let start = query_pointer(&observer, root)?;
    let endpoint = RootPoint::new(160, 120)?;
    let interpolated = plan_motion(
        start,
        endpoint,
        MotionOptions::new(
            MotionCurve::Smooth,
            Some(240),
            MotionPolicy::default(),
            false,
        )?,
    )?;
    run_completed(
        &handle,
        "interpolated_move",
        InputAction::Move(MoveAction::new(interpolated)),
    )?;
    action_count += 1;

    let instant_endpoint = RootPoint::new(190, 150)?;
    let instant = plan_motion(endpoint, instant_endpoint, MotionOptions::instant(false))?;
    run_completed(
        &handle,
        "instant_move",
        InputAction::Move(MoveAction::new(instant)),
    )?;
    action_count += 1;

    let click_endpoint = RootPoint::new(220, 180)?;
    let click_move = plan_motion(
        instant_endpoint,
        click_endpoint,
        MotionOptions::new(
            MotionCurve::Linear,
            Some(120),
            MotionPolicy::default(),
            false,
        )?,
    )?;
    run_completed(
        &handle,
        "double_click",
        InputAction::Click(ClickAction::new(
            Some(click_move),
            LogicalButton::Left,
            2,
            30,
            45,
            100,
            DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
        )?),
    )?;
    action_count += 1;

    let drag_endpoint = RootPoint::new(340, 260)?;
    let drag = plan_waypoint_motion(
        click_endpoint,
        &[
            RootPoint::new(260, 190)?,
            RootPoint::new(300, 230)?,
            drag_endpoint,
        ],
        MotionCurve::Linear,
        MotionPolicy::default(),
        false,
        WaypointDurationPolicy::Total(300),
    )?;
    run_completed(
        &handle,
        "drag",
        InputAction::Drag(DragAction::new(drag, LogicalButton::Left, 35, 40)?),
    )?;
    action_count += 1;

    for (name, direction) in [
        ("scroll_down", ScrollDirection::Down),
        ("scroll_right", ScrollDirection::Right),
    ] {
        run_completed(
            &handle,
            name,
            InputAction::Scroll(ScrollAction::new(direction, 2, 55)?),
        )?;
        action_count += 1;
    }

    // Keep a delayed action in flight while a different X connection performs
    // a reply-producing observation. This proves XTEST timing does not make
    // independent observation traffic wait for command completion.
    let delayed_click = InputAction::Click(ClickAction::new(
        None,
        LogicalButton::Left,
        1,
        700,
        40,
        0,
        DEFAULT_DOUBLE_CLICK_THRESHOLD_MS,
    )?);
    let mut pending = submit(&handle, delayed_click, CancellationToken::new())?;
    thread::sleep(Duration::from_millis(80));
    let observation_started = Instant::now();
    let observed = query_pointer(&observer, root)?;
    let observation_elapsed = observation_started.elapsed();
    let while_action_pending = matches!(
        pending.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    write_record(&Record::IndependentObservation {
        elapsed_ms: observation_elapsed.as_millis(),
        pointer: observed.into(),
        while_action_pending,
    })?;
    if !while_action_pending {
        return Err("delayed action completed before independent observation".into());
    }
    if observation_elapsed > Duration::from_millis(250) {
        return Err(format!(
            "independent QueryPointer took {} ms during XTEST delay",
            observation_elapsed.as_millis()
        )
        .into());
    }
    write_completed("delayed_click", receive(pending)?)?;
    action_count += 1;

    // Cancellation is requested while the first 500 ms waypoint segment is
    // executing. The actor may stop only after that segment's observation
    // boundary and must release the held drag button before returning.
    let cancellation_start = query_pointer(&observer, root)?;
    let cancelled_drag = plan_waypoint_motion(
        cancellation_start,
        &[
            RootPoint::new(390, 300)?,
            RootPoint::new(430, 330)?,
            RootPoint::new(470, 360)?,
        ],
        MotionCurve::Linear,
        MotionPolicy::default(),
        false,
        WaypointDurationPolicy::PerSegment(vec![500, 300, 300]),
    )?;
    let cancellation = CancellationToken::new();
    let pending = submit(
        &handle,
        InputAction::Drag(DragAction::new(
            cancelled_drag,
            LogicalButton::Left,
            20,
            20,
        )?),
        cancellation.clone(),
    )?;
    thread::sleep(Duration::from_millis(120));
    cancellation.cancel();
    let cancelled = receive(pending)?;
    match cancelled {
        Ok(outcome) if outcome.kind == InputOutcomeKind::CancelledAfterEffect => {
            write_record(&Record::CancellationBoundary {
                result: "cancelled_after_effect",
                events_emitted: outcome.events_emitted,
                completed_units: outcome.completed_units,
                observed_pointer: outcome.observed_pointer.map(Into::into),
                cleanup_succeeded: None,
            })?;
        }
        Err(failure) => {
            write_record(&Record::CancellationBoundary {
                result: "failure",
                events_emitted: failure.events_emitted,
                completed_units: failure.completed_units,
                observed_pointer: failure.last_observed_pointer.map(Into::into),
                cleanup_succeeded: failure.cleanup.as_ref().map(|report| report.succeeded()),
            })?;
            return Err(failure.into());
        }
        Ok(outcome) => {
            return Err(format!("unexpected cancellation outcome: {:?}", outcome.kind).into());
        }
    }
    action_count += 1;

    let keyboard_actions = run_keyboard_scenario(&handle, &observer)?;

    let shutdown = handle.shutdown().blocking_recv()??;
    if !matches!(shutdown, xenoteer_x11::input::ControlOutcome::Shutdown(_)) {
        return Err("shutdown control returned the wrong response variant".into());
    }
    let exit = join.join();
    if exit != InputActorExit::Stopped {
        return Err(format!("input actor exited as {exit:?}").into());
    }
    write_record(&Record::Complete {
        scenario: SCENARIO,
        window: options.window,
        actor_exit: "stopped",
        pointer_actions: action_count,
        keyboard_actions,
    })?;
    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut window = None;
    let mut scenario = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--window" => {
                let value = args.next().ok_or("--window requires an X11 window id")?;
                window = Some(parse_window(&value)?);
            }
            "--scenario" => {
                scenario = Some(args.next().ok_or("--scenario requires a name")?);
            }
            _ => return Err(usage().into()),
        }
    }
    Ok(Options {
        window: window.ok_or("--window is required")?,
        scenario: scenario.ok_or("--scenario is required")?,
    })
}

fn parse_window(value: &str) -> Result<u32, Box<dyn Error>> {
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(u32::from_str_radix(hex, 16)?)
    } else {
        Ok(value.parse()?)
    }
}

fn usage() -> &'static str {
    "usage: phase1-input --window WINDOW --scenario conformance"
}

fn query_pointer<C: x11rb::connection::Connection>(
    connection: &C,
    root: u32,
) -> Result<RootPoint, Box<dyn Error>> {
    let reply = connection.query_pointer(root)?.reply()?;
    Ok(RootPoint::new(
        i32::from(reply.root_x),
        i32::from(reply.root_y),
    )?)
}

fn submit(
    handle: &InputActorHandle,
    action: InputAction,
    cancellation: CancellationToken,
) -> Result<tokio::sync::oneshot::Receiver<Result<InputOutcome, InputFailure>>, Box<dyn Error>> {
    Ok(handle.try_submit(
        ActionContext::new(CommandId::new(), None),
        action,
        cancellation,
    )?)
}

fn receive(
    receiver: tokio::sync::oneshot::Receiver<Result<InputOutcome, InputFailure>>,
) -> Result<Result<InputOutcome, InputFailure>, Box<dyn Error>> {
    Ok(receiver.blocking_recv()?)
}

fn run_completed(
    handle: &InputActorHandle,
    name: &'static str,
    action: InputAction,
) -> Result<(), Box<dyn Error>> {
    write_completed(
        name,
        receive(submit(handle, action, CancellationToken::new())?)?,
    )
}

fn write_completed(
    name: &'static str,
    result: Result<InputOutcome, InputFailure>,
) -> Result<(), Box<dyn Error>> {
    let outcome = result?;
    write_completed_outcome(name, outcome)
}

fn write_completed_outcome(
    name: &'static str,
    outcome: InputOutcome,
) -> Result<(), Box<dyn Error>> {
    if outcome.kind != InputOutcomeKind::Completed {
        return Err(format!("{name} did not complete: {:?}", outcome.kind).into());
    }
    let (
        keyboard_bindings,
        text_scalar_count,
        requested_text_mode,
        current_layout_scalars,
        temporary_mapping_scalars,
        temporary_mappings_installed,
        temporary_mappings_restored,
        temporary_mapping_restoration_proven,
    ) = outcome.keyboard.as_ref().map_or(
        (None, None, None, None, None, None, None, None),
        |evidence| {
            (
                Some(evidence.bindings.len()),
                evidence.text_scalar_count,
                evidence.requested_text_mode.map(text_mode_name),
                Some(evidence.current_layout_scalars),
                Some(evidence.temporary_mapping_scalars),
                Some(evidence.temporary_mappings_installed),
                Some(evidence.temporary_mappings_restored),
                evidence.temporary_mapping_restoration_proven,
            )
        },
    );
    let (effect_evidence, effect_provisional, effect_confirmed) = match &outcome.effects {
        InputEffectEvidence::Journal(_) => ("journal", None, None),
        InputEffectEvidence::RedactedKeyboard {
            provisional,
            confirmed,
        } => ("redacted_keyboard", Some(*provisional), Some(*confirmed)),
    };
    write_record(&Record::Action {
        name,
        result: "completed",
        events_emitted: outcome.events_emitted,
        completed_units: outcome.completed_units,
        requested_pointer: outcome.requested_pointer.map(Into::into),
        observed_pointer: outcome.observed_pointer.map(Into::into),
        keyboard_bindings,
        text_scalar_count,
        requested_text_mode,
        current_layout_scalars,
        temporary_mapping_scalars,
        temporary_mappings_installed,
        temporary_mappings_restored,
        temporary_mapping_restoration_proven,
        effect_evidence,
        effect_provisional,
        effect_confirmed,
    })
}

fn text_mode_name(mode: PhysicalTextMode) -> &'static str {
    match mode {
        PhysicalTextMode::CurrentLayout => "current_layout",
        PhysicalTextMode::ExtendedTemporaryMapping => "extended_temporary_mapping",
    }
}

#[cfg(not(feature = "native-xkbcommon"))]
fn run_keyboard_scenario<C: x11rb::connection::Connection>(
    _handle: &InputActorHandle,
    _connection: &C,
) -> Result<usize, Box<dyn Error>> {
    Ok(0)
}

#[cfg(not(feature = "native-xkbcommon"))]
fn prime_keyboard_for_live_conformance<C: x11rb::connection::Connection>(
    _handle: &InputActorHandle,
    _connection: &C,
) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(feature = "native-xkbcommon")]
fn prime_keyboard_for_live_conformance<C: x11rb::connection::Connection>(
    handle: &InputActorHandle,
    connection: &C,
) -> Result<(), Box<dyn Error>> {
    let keycode = find_unused_no_symbol_keycode(connection)?;
    let action = KeyboardAction::press(KeyIdentifier::Raw(keycode), 0)?;
    let receiver = handle.try_submit_keyboard(
        ActionContext::new(CommandId::new(), None),
        action,
        CancellationToken::new(),
    )?;
    match receive(receiver)? {
        Ok(outcome) if outcome.kind == InputOutcomeKind::Completed => {
            if outcome.events_emitted != 2 {
                return Err(format!(
                    "diagnostic keyboard prime emitted {} events instead of 2",
                    outcome.events_emitted
                )
                .into());
            }
            write_record(&Record::KeyboardPrime {
                keycode,
                result: "completed",
                events_emitted: outcome.events_emitted,
                cleanup_succeeded: None,
            })
        }
        Err(failure)
            if failure.kind
                == xenoteer_x11::input::InputFailureKind::KeyboardMappingChangedAfterEffect
                && failure.progress_known
                && failure.events_emitted == 2
                && failure
                    .cleanup
                    .as_ref()
                    .is_some_and(|cleanup| cleanup.succeeded()) =>
        {
            write_record(&Record::KeyboardPrime {
                keycode,
                result: "mapping_changed_after_effect",
                events_emitted: failure.events_emitted,
                cleanup_succeeded: Some(true),
            })
        }
        Err(failure) => Err(failure.into()),
        Ok(outcome) => Err(format!(
            "unexpected diagnostic keyboard prime outcome: {:?}",
            outcome.kind
        )
        .into()),
    }
}

#[cfg(feature = "native-xkbcommon")]
fn find_unused_no_symbol_keycode<C: x11rb::connection::Connection>(
    connection: &C,
) -> Result<u8, Box<dyn Error>> {
    let first = connection.setup().min_keycode;
    let last = connection.setup().max_keycode;
    let count = last
        .checked_sub(first)
        .and_then(|difference| difference.checked_add(1))
        .ok_or("X server advertised an invalid core keycode range")?;
    let mapping = connection.get_keyboard_mapping(first, count)?.reply()?;
    let modifier_map = connection.get_modifier_mapping()?.reply()?;
    let pressed = connection.query_keymap()?.reply()?;
    let symbols_per_key = usize::from(mapping.keysyms_per_keycode);

    for keycode in (first..=last).rev() {
        let mapping_offset = usize::from(keycode - first).saturating_mul(symbols_per_key);
        let mapping_end = mapping_offset.saturating_add(symbols_per_key);
        let all_no_symbol = mapping
            .keysyms
            .get(mapping_offset..mapping_end)
            .is_some_and(|symbols| {
                !symbols.is_empty() && symbols.iter().all(|symbol| *symbol == 0)
            });
        let is_modifier = modifier_map.keycodes.contains(&keycode);
        let byte = usize::from(keycode / 8);
        let bit = 1_u8 << (keycode % 8);
        let is_pressed = pressed.keys.get(byte).is_some_and(|value| value & bit != 0);
        if all_no_symbol && !is_modifier && !is_pressed {
            return Ok(keycode);
        }
    }
    Err(
        "no unused, nonmodifier, unpressed NoSymbol keycode is available for Xvfb conformance"
            .into(),
    )
}

#[cfg(feature = "native-xkbcommon")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct CoreKeyMappingSnapshot {
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
}

#[cfg(feature = "native-xkbcommon")]
fn read_core_key_mapping<C: x11rb::connection::Connection>(
    connection: &C,
    keycode: u8,
) -> Result<CoreKeyMappingSnapshot, Box<dyn Error>> {
    let mapping = connection.get_keyboard_mapping(keycode, 1)?.reply()?;
    Ok(CoreKeyMappingSnapshot {
        keysyms_per_keycode: mapping.keysyms_per_keycode,
        keysyms: mapping.keysyms,
    })
}

#[cfg(feature = "native-xkbcommon")]
fn keycode_safety<C: x11rb::connection::Connection>(
    connection: &C,
    keycode: u8,
) -> Result<(bool, bool), Box<dyn Error>> {
    let modifier_map = connection.get_modifier_mapping()?.reply()?;
    let pressed = connection.query_keymap()?.reply()?;
    let byte = usize::from(keycode / 8);
    let bit = 1_u8 << (keycode % 8);
    let is_pressed = pressed.keys.get(byte).is_some_and(|value| value & bit != 0);
    Ok((!is_pressed, !modifier_map.keycodes.contains(&keycode)))
}

#[cfg(feature = "native-xkbcommon")]
fn run_keyboard_scenario<C: x11rb::connection::Connection>(
    handle: &InputActorHandle,
    observer: &C,
) -> Result<usize, Box<dyn Error>> {
    let actions = [
        (
            "named_enter",
            KeyboardAction::press(KeyIdentifier::Named(NamedKey::Enter), 35)?,
        ),
        (
            "scalar_x",
            KeyboardAction::press(KeyIdentifier::Scalar('x'), 35)?,
        ),
        (
            "chord_control_a",
            KeyboardAction::chord(
                &[
                    KeyIdentifier::Named(NamedKey::ControlLeft),
                    KeyIdentifier::Scalar('a'),
                ],
                45,
            )?,
        ),
        (
            "keyboard_sequence",
            KeyboardAction::sequence(&[
                KeyboardSequenceStep::press(KeyIdentifier::Named(NamedKey::Escape), 25, 0)?,
                KeyboardSequenceStep::press(KeyIdentifier::Scalar('b'), 25, 50)?,
                KeyboardSequenceStep::chord(
                    &[
                        KeyIdentifier::Named(NamedKey::ShiftLeft),
                        KeyIdentifier::Scalar('c'),
                    ],
                    30,
                    50,
                )?,
            ])?,
        ),
        (
            "physical_text_current_layout",
            KeyboardAction::physical_text("Az1!", PhysicalTextMode::CurrentLayout, 20)?,
        ),
    ];

    let action_count = actions.len();
    for (name, action) in actions {
        let receiver = handle.try_submit_keyboard(
            ActionContext::new(CommandId::new(), None),
            action,
            CancellationToken::new(),
        )?;
        write_completed(name, receive(receiver)?)?;
    }

    // Prove the highest-risk extended text path from outside the actor. The
    // observer snapshots every core keysym slot of the same deterministic
    // highest safe reservation the native model will choose, then compares
    // the complete mapping byte-for-byte after the actor reports restoration.
    let temporary_keycode = find_unused_no_symbol_keycode(observer)?;
    let before = read_core_key_mapping(observer, temporary_keycode)?;
    let before_all_no_symbol =
        !before.keysyms.is_empty() && before.keysyms.iter().all(|keysym| *keysym == 0);
    let (before_unpressed, before_nonmodifier) = keycode_safety(observer, temporary_keycode)?;
    if !before_all_no_symbol || !before_unpressed || !before_nonmodifier {
        return Err("temporary mapping candidate was not safe before submission".into());
    }

    let receiver = handle.try_submit_keyboard(
        ActionContext::new(CommandId::new(), None),
        KeyboardAction::physical_text("\u{2603}", PhysicalTextMode::ExtendedTemporaryMapping, 0)?,
        CancellationToken::new(),
    )?;
    let outcome = receive(receiver)??;
    require_extended_temporary_evidence(&outcome)?;

    let after = read_core_key_mapping(observer, temporary_keycode)?;
    let after_exact_match = after == before;
    let (after_unpressed, after_nonmodifier) = keycode_safety(observer, temporary_keycode)?;
    if !after_exact_match || !after_unpressed || !after_nonmodifier {
        return Err("temporary mapping restoration failed independent proof".into());
    }

    write_completed_outcome("physical_text_extended_temporary", outcome)?;
    write_record(&Record::TemporaryMappingProof {
        keycode: temporary_keycode,
        keysyms_per_keycode: before.keysyms_per_keycode,
        mapping_word_count: before.keysyms.len(),
        before_all_no_symbol,
        before_unpressed,
        before_nonmodifier,
        after_exact_match,
        after_unpressed,
        after_nonmodifier,
    })?;

    Ok(action_count + 1)
}

#[cfg(feature = "native-xkbcommon")]
fn require_extended_temporary_evidence(outcome: &InputOutcome) -> Result<(), Box<dyn Error>> {
    if outcome.kind != InputOutcomeKind::Completed
        || outcome.events_emitted != 2
        || outcome.completed_units != 1
    {
        return Err(format!(
            "extended temporary text returned incomplete aggregate progress: {:?}",
            outcome.kind
        )
        .into());
    }
    let evidence = outcome
        .keyboard
        .as_ref()
        .ok_or("extended temporary text omitted keyboard evidence")?;
    if !evidence.bindings.is_empty()
        || evidence.text_scalar_count != Some(1)
        || evidence.requested_text_mode != Some(PhysicalTextMode::ExtendedTemporaryMapping)
        || evidence.current_layout_scalars != 0
        || evidence.temporary_mapping_scalars != 1
        || evidence.temporary_mappings_installed != 1
        || evidence.temporary_mappings_restored != 1
        || evidence.temporary_mapping_restoration_proven != Some(true)
    {
        return Err("extended temporary text evidence was incomplete or inconsistent".into());
    }
    match &outcome.effects {
        InputEffectEvidence::RedactedKeyboard {
            provisional: 0,
            confirmed,
        } if *confirmed == outcome.events_emitted => Ok(()),
        _ => Err("extended temporary text effect evidence was not exact and redacted".into()),
    }
}

fn write_record(record: &Record<'_>) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, record)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
