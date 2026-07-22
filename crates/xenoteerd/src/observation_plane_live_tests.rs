//! Executable live-X11 adversarial proofs for the daemon observation boundary.

use std::{error::Error, io, sync::mpsc::RecvTimeoutError, time::Duration};

use tokio::sync::oneshot;
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, Window,
        WindowClass,
    },
    wrapper::ConnectionExt as _,
};
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, MAX_WINDOW_PAGE_LIMIT, WindowListPage, WindowListRequest,
    WindowOrder, WindowPredicate, WindowQueryPage, WindowQueryRequest, WindowResolveRequest,
    WindowSingleMatchPolicy, WindowSnapshotRequest, WindowSnapshotResult, WindowSnapshotTarget,
    WindowStringMatch, WindowTextField, WindowWaitPredicate, WindowWaitRequest, WindowWaitResult,
    WindowWaitSelectorQuantifier, WindowWaitStatus, WindowWaitTarget,
};
use xenoteer_server::ControlPlaneError;
use xenoteer_x11::{
    RawWindowControlObservation, RawWindowControlOperation, RawWindowControlOutcome,
    RawWindowControlRequest, RawWindowRevalidationError, WindowControlActorExit,
    WindowControlActorFailureKind, spawn_window_control_actor,
};

use super::*;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);
const ACTOR_TIMEOUT: Duration = Duration::from_secs(2);

fn display() -> Result<String, Box<dyn Error>> {
    Ok(std::env::var("XENOTEER_TEST_DISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .map_err(|_| "XENOTEER_TEST_DISPLAY or DISPLAY is required")?)
}

fn exact_title(title: &str) -> WindowSelector {
    WindowSelector::Predicate {
        predicate: WindowPredicate::Text {
            field: WindowTextField::Title,
            matcher: WindowStringMatch::Exact {
                value: title.to_owned(),
                case_sensitive: true,
            },
        },
    }
}

fn control_error(error: ControlPlaneError) -> io::Error {
    io::Error::other(format!("observation request failed: {error:?}"))
}

async fn response<T>(
    receiver: oneshot::Receiver<Result<T, ControlPlaneError>>,
) -> Result<T, Box<dyn Error>> {
    tokio::time::timeout(RESPONSE_TIMEOUT, receiver)
        .await
        .map_err(|_| io::Error::other("observation response exceeded its outer deadline"))?
        .map_err(|_| io::Error::other("observation actor dropped its response"))?
        .map_err(control_error)
        .map_err(Into::into)
}

fn submit_list(
    service: &DaemonObservationService,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<oneshot::Receiver<Result<WindowListPage, ControlPlaneError>>, io::Error> {
    let (sender, receiver) = oneshot::channel();
    service
        .submit(ModelRequest::List {
            principal: "live-x11-adversary".to_owned(),
            request: WindowListRequest {
                desktop_id,
                desktop_generation,
                limit: MAX_WINDOW_PAGE_LIMIT,
                order: WindowOrder::XidAscending,
                cursor: None,
            },
            response: sender,
        })
        .map_err(control_error)?;
    Ok(receiver)
}

fn submit_query(
    service: &DaemonObservationService,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    title: &str,
) -> Result<oneshot::Receiver<Result<WindowQueryPage, ControlPlaneError>>, io::Error> {
    let (sender, receiver) = oneshot::channel();
    service
        .submit(ModelRequest::Query {
            principal: "live-x11-adversary".to_owned(),
            request: WindowQueryRequest {
                desktop_id,
                desktop_generation,
                selector: exact_title(title),
                order: WindowOrder::XidAscending,
                limit: MAX_WINDOW_PAGE_LIMIT,
                cursor: None,
            },
            response: sender,
        })
        .map_err(control_error)?;
    Ok(receiver)
}

fn submit_resolve(
    service: &DaemonObservationService,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    title: &str,
) -> Result<
    oneshot::Receiver<Result<xenoteer_protocol::WindowResolveResult, ControlPlaneError>>,
    io::Error,
> {
    let (sender, receiver) = oneshot::channel();
    service
        .submit(ModelRequest::Resolve {
            principal: "live-x11-adversary".to_owned(),
            request: WindowResolveRequest {
                desktop_id,
                desktop_generation,
                selector: exact_title(title),
                order: WindowOrder::XidAscending,
                match_policy: WindowSingleMatchPolicy::ExactlyOne,
            },
            response: sender,
        })
        .map_err(control_error)?;
    Ok(receiver)
}

fn submit_wait(
    service: &DaemonObservationService,
    request: WindowWaitRequest,
) -> Result<oneshot::Receiver<Result<WindowWaitResult, ControlPlaneError>>, io::Error> {
    let (sender, receiver) = oneshot::channel();
    service
        .submit(ModelRequest::Wait {
            principal: "live-x11-adversary".to_owned(),
            request,
            response: sender,
        })
        .map_err(control_error)?;
    Ok(receiver)
}

fn submit_snapshot(
    service: &DaemonObservationService,
    request: WindowSnapshotRequest,
) -> Result<oneshot::Receiver<Result<WindowSnapshotResult, ControlPlaneError>>, io::Error> {
    let (sender, receiver) = oneshot::channel();
    service
        .submit(ModelRequest::Snapshot {
            principal: "live-x11-adversary".to_owned(),
            request,
            response: sender,
        })
        .map_err(control_error)?;
    Ok(receiver)
}

#[derive(Clone, Copy)]
struct LiveWindowContext {
    root: Window,
    root_depth: u8,
    root_visual: u32,
    utf8_string: Atom,
    net_wm_name: Atom,
}

fn create_titled_window<C: Connection>(
    connection: &C,
    context: LiveWindowContext,
    window: Window,
    x: i16,
    title: &str,
) -> Result<(), Box<dyn Error>> {
    connection
        .create_window(
            context.root_depth,
            window,
            context.root,
            x,
            40,
            180,
            120,
            0,
            WindowClass::INPUT_OUTPUT,
            context.root_visual,
            &CreateWindowAux::new(),
        )?
        .check()?;
    connection
        .change_property8(
            PropMode::REPLACE,
            window,
            context.net_wm_name,
            context.utf8_string,
            title.as_bytes(),
        )?
        .check()?;
    connection
        .change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?
        .check()?;
    connection.map_window(window)?.check()?;
    connection.get_input_focus()?.reply()?;
    Ok(())
}

fn selector_wait(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    title: &str,
    after_revision: Option<xenoteer_protocol::WindowModelRevision>,
) -> WindowWaitRequest {
    WindowWaitRequest {
        desktop_id,
        desktop_generation,
        target: WindowWaitTarget::Selector {
            selector: exact_title(title),
            quantifier: WindowWaitSelectorQuantifier::Any,
        },
        predicate: WindowWaitPredicate::Exists,
        after_revision,
        timeout_ms: 2_000,
    }
}

fn reference_closed_wait(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    window: xenoteer_protocol::WindowRef,
) -> WindowWaitRequest {
    WindowWaitRequest {
        desktop_id,
        desktop_generation,
        target: WindowWaitTarget::Reference { window },
        predicate: WindowWaitPredicate::Closed,
        after_revision: None,
        timeout_ms: 2_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
async fn live_ambiguity_wait_races_and_reused_xid_fail_closed_before_focus_effect()
-> Result<(), Box<dyn Error>> {
    let display = display()?;
    let producer = xenoteer_x11::connect(&display)?;
    let screen = &producer.connection.setup().roots[producer.info.screen_index];
    let root = screen.root;
    let root_depth = screen.root_depth;
    let root_visual = screen.root_visual;
    let utf8_string = producer
        .connection
        .intern_atom(false, b"UTF8_STRING")?
        .reply()?
        .atom;
    let net_wm_name = producer
        .connection
        .intern_atom(false, b"_NET_WM_NAME")?
        .reply()?
        .atom;
    let window_context = LiveWindowContext {
        root,
        root_depth,
        root_visual,
        utf8_string,
        net_wm_name,
    };

    let duplicate_title = "Xenoteer live ambiguity sentinel";
    let first_xid = producer.connection.generate_id()?;
    let second_xid = producer.connection.generate_id()?;
    create_titled_window(
        &producer.connection,
        window_context,
        first_xid,
        20,
        duplicate_title,
    )?;
    create_titled_window(
        &producer.connection,
        window_context,
        second_xid,
        240,
        duplicate_title,
    )?;

    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let (service, shutdown, join) = spawn_live_observation_service(
        &display,
        desktop_id,
        desktop_generation,
        WindowModelLimits::default(),
        ObservationServiceSettings::default(),
    )?;
    assert_eq!(service.health(), ObservationServiceState::Healthy);

    let duplicate_page = response(submit_query(
        service.as_ref(),
        desktop_id,
        desktop_generation,
        duplicate_title,
    )?)
    .await?;
    assert_eq!(duplicate_page.windows.len(), 2);
    let old_reference = duplicate_page
        .windows
        .iter()
        .find(|entry| entry.snapshot.window.xid == first_xid)
        .ok_or("first duplicate XID was absent from the live model")?
        .snapshot
        .window
        .clone();

    let ambiguous = submit_resolve(
        service.as_ref(),
        desktop_id,
        desktop_generation,
        duplicate_title,
    )?;
    assert_eq!(ambiguous.await?, Err(ControlPlaneError::LeaseConflict));

    // Enqueue the wait first, then map immediately. If the actor evaluates
    // before MapNotify it must register; if MapNotify wins it must match the
    // newer revision. Either interleaving is an executable check/register race.
    let boundary = response(submit_list(
        service.as_ref(),
        desktop_id,
        desktop_generation,
    )?)
    .await?
    .snapshot_revision;
    let racing_title = "Xenoteer live wait race sentinel";
    let racing_wait = submit_wait(
        service.as_ref(),
        selector_wait(desktop_id, desktop_generation, racing_title, Some(boundary)),
    )?;
    let racing_xid = producer.connection.generate_id()?;
    create_titled_window(
        &producer.connection,
        window_context,
        racing_xid,
        460,
        racing_title,
    )?;
    let raced = response(racing_wait).await?;
    assert_eq!(raced.status, WindowWaitStatus::Matched);
    assert!(raced.predicate_satisfied);
    assert!(raced.evaluated_revision > boundary);
    assert_eq!(raced.windows.len(), 1);
    assert_eq!(raced.windows[0].snapshot.window.xid, racing_xid);

    let racing_closed = submit_wait(
        service.as_ref(),
        reference_closed_wait(
            desktop_id,
            desktop_generation,
            raced.windows[0].snapshot.window.clone(),
        ),
    )?;
    producer.connection.destroy_window(racing_xid)?.check()?;
    producer.connection.get_input_focus()?.reply()?;
    let raced_closed = response(racing_closed).await?;
    assert_eq!(raced_closed.status, WindowWaitStatus::Matched);
    assert!(raced_closed.predicate_satisfied);

    let old_closed = submit_wait(
        service.as_ref(),
        reference_closed_wait(desktop_id, desktop_generation, old_reference.clone()),
    )?;
    producer.connection.destroy_window(first_xid)?.check()?;
    producer.connection.get_input_focus()?.reply()?;
    let old_closed = response(old_closed).await?;
    assert_eq!(old_closed.status, WindowWaitStatus::Matched);

    let replacement_title = "Xenoteer reused XID replacement";
    let replacement_wait = submit_wait(
        service.as_ref(),
        selector_wait(
            desktop_id,
            desktop_generation,
            replacement_title,
            Some(old_closed.evaluated_revision),
        ),
    )?;
    create_titled_window(
        &producer.connection,
        window_context,
        first_xid,
        20,
        replacement_title,
    )?;
    let replacement = response(replacement_wait).await?;
    let replacement_reference = replacement
        .windows
        .first()
        .ok_or("reused XID replacement was absent")?
        .snapshot
        .window
        .clone();
    assert_eq!(replacement_reference.xid, old_reference.xid);
    assert_ne!(
        replacement_reference.observed_generation,
        old_reference.observed_generation
    );
    assert_ne!(
        replacement_reference.identity_hash,
        old_reference.identity_hash
    );

    let stale_snapshot = submit_snapshot(
        service.as_ref(),
        WindowSnapshotRequest {
            desktop_id,
            desktop_generation,
            target: WindowSnapshotTarget::Reference {
                window: old_reference.clone(),
            },
        },
    )?;
    assert_eq!(stale_snapshot.await?, Err(ControlPlaneError::NotFound));
    assert_eq!(
        service.revalidate_exact_blocking(old_reference.clone(), ACTOR_TIMEOUT),
        Err(ControlPlaneError::NotFound)
    );

    // A stale reference with the same live numeric XID must stop before the
    // raw actor's SetInputFocus fallback. This turns identity fencing into an
    // observable no-effect proof on the real X server.
    producer
        .connection
        .set_input_focus(InputFocus::PARENT, second_xid, x11rb::CURRENT_TIME)?
        .check()?;
    producer.connection.get_input_focus()?.reply()?;
    let (control, control_join) = spawn_window_control_actor(&display)?;
    let stale_service = service.clone();
    let stale_reference = old_reference.clone();
    let rejected = control.try_submit(
        RawWindowControlRequest {
            target: first_xid,
            operation: RawWindowControlOperation::Activate {
                timestamp: x11rb::CURRENT_TIME,
                switch_workspace: None,
                allow_set_input_focus: true,
            },
            timeout: Duration::from_millis(250),
        },
        move || match stale_service.revalidate_exact_blocking(stale_reference, ACTOR_TIMEOUT) {
            Err(ControlPlaneError::NotFound) => Err(RawWindowRevalidationError::StaleReference),
            Ok(_) => Ok(()),
            Err(_) => Err(RawWindowRevalidationError::Rejected),
        },
    )?;
    let failure = match rejected.recv_timeout(ACTOR_TIMEOUT)? {
        Err(failure) => failure,
        Ok(_) => return Err("stale live birth unexpectedly reached the raw X11 backend".into()),
    };
    assert_eq!(failure.kind, WindowControlActorFailureKind::StaleReference);
    assert_eq!(
        producer.connection.get_input_focus()?.reply()?.focus,
        second_xid,
        "stale same-XID command retargeted core input focus"
    );
    control.shutdown().recv_timeout(ACTOR_TIMEOUT)??;
    assert_eq!(control_join.join(), WindowControlActorExit::Stopped);

    shutdown.request();
    assert_eq!(join.join(), ObservationServiceExit::Stopped);
    producer.connection.destroy_window(first_xid)?.check()?;
    producer.connection.destroy_window(second_xid)?.check()?;
    Ok(())
}

struct RootPropertyCleanup {
    display: String,
    properties: Vec<Atom>,
}

impl Drop for RootPropertyCleanup {
    fn drop(&mut self) {
        let Ok(opened) = xenoteer_x11::connect(&self.display) else {
            return;
        };
        for property in &self.properties {
            let _ignored = opened
                .connection
                .delete_property(opened.info.root, *property);
        }
        let _ignored = opened.connection.flush();
    }
}

#[test]
#[ignore = "requires authenticated Xvfb; run tests/platform/run-x11-spikes.sh"]
fn ewmh_activation_refusal_is_bounded_and_never_forces_focus_without_opt_in()
-> Result<(), Box<dyn Error>> {
    let display = display()?;
    let producer = xenoteer_x11::connect(&display)?;
    let screen = &producer.connection.setup().roots[producer.info.screen_index];
    let baseline = producer.connection.generate_id()?;
    let refusing = producer.connection.generate_id()?;
    for (window, x) in [(baseline, 50), (refusing, 300)] {
        producer
            .connection
            .create_window(
                screen.root_depth,
                window,
                screen.root,
                x,
                250,
                180,
                120,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new(),
            )?
            .check()?;
        producer.connection.map_window(window)?.check()?;
    }

    let net_supported = producer
        .connection
        .intern_atom(false, b"_NET_SUPPORTED")?
        .reply()?
        .atom;
    let net_active_window = producer
        .connection
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let wm_hints = producer
        .connection
        .intern_atom(false, b"WM_HINTS")?
        .reply()?
        .atom;
    let _cleanup = RootPropertyCleanup {
        display: display.clone(),
        properties: vec![net_supported, net_active_window],
    };

    // ICCCM InputHint is present and false: a cooperating manager may make
    // the client active, but must not force core focus into this window.
    producer
        .connection
        .change_property32(
            PropMode::REPLACE,
            refusing,
            wm_hints,
            wm_hints,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0],
        )?
        .check()?;
    producer
        .connection
        .change_property32(
            PropMode::REPLACE,
            screen.root,
            net_supported,
            AtomEnum::ATOM,
            &[net_active_window],
        )?
        .check()?;
    producer
        .connection
        .change_property32(
            PropMode::REPLACE,
            screen.root,
            net_active_window,
            AtomEnum::WINDOW,
            &[baseline],
        )?
        .check()?;
    producer
        .connection
        .set_input_focus(InputFocus::PARENT, baseline, x11rb::CURRENT_TIME)?
        .check()?;
    producer.connection.get_input_focus()?.reply()?;

    let hints = producer
        .connection
        .get_property(false, refusing, wm_hints, wm_hints, 0, 9)?
        .reply()?;
    let hint_values = hints.value32().ok_or("WM_HINTS was not format 32")?;
    let hint_values = hint_values.collect::<Vec<_>>();
    assert_eq!(&hint_values[..2], &[1, 0]);

    let (control, join) = spawn_window_control_actor(&display)?;
    let evidence = control
        .try_submit(
            RawWindowControlRequest {
                target: refusing,
                operation: RawWindowControlOperation::Activate {
                    timestamp: x11rb::CURRENT_TIME,
                    switch_workspace: None,
                    allow_set_input_focus: false,
                },
                timeout: Duration::from_millis(200),
            },
            || Ok(()),
        )?
        .recv_timeout(ACTOR_TIMEOUT)??;
    assert_eq!(evidence.outcome, RawWindowControlOutcome::TimedOut);
    let RawWindowControlObservation::Activation {
        active,
        focused,
        focus_within_target,
        ..
    } = evidence.observed
    else {
        return Err("activation refusal returned the wrong evidence family".into());
    };
    assert_eq!(active, Some(baseline));
    assert_eq!(focused, Some(baseline));
    assert!(!focus_within_target);
    assert_eq!(
        producer.connection.get_input_focus()?.reply()?.focus,
        baseline,
        "non-opted-in activation forced input focus after manager refusal"
    );

    control.shutdown().recv_timeout(ACTOR_TIMEOUT)??;
    assert_eq!(join.join(), WindowControlActorExit::Stopped);
    producer.connection.destroy_window(refusing)?.check()?;
    producer.connection.destroy_window(baseline)?.check()?;
    Ok(())
}

// Keep the explicit timeout error variant imported and checked by the compiler;
// actor replies in these tests must never silently turn disconnect into timeout.
const _: Option<RecvTimeoutError> = None;
