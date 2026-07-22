//! Phase-2 desktop capability supervisor and input-actor lifetime owner.

use std::{future::Future, net::SocketAddr, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Instant, interval_at, timeout},
};
use tokio_util::sync::CancellationToken;
use xenoteer_atspi::LiveAtspiProbe;
use xenoteer_core::{Config, input::InputHealth};
use xenoteer_protocol::DesktopGeneration;
use xenoteer_server::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot};
use xenoteer_x11::{
    DesktopProbeExpectation,
    input::{
        ActorThreadState, ControlOutcome, InputActorExit, InputActorHandle, InputActorJoin,
        spawn_input_actor,
    },
    keyboard::KeyboardModelAvailability,
    probe_desktop, probe_desktop_steady_state,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const MONITOR_INTERVAL: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const BLOCKING_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINAL_BLOCKING_EXIT_CODE: i32 = 70;

/// Non-secret startup values copied from validated configuration.
#[derive(Clone, Debug)]
pub(crate) struct DesktopProbeSpec {
    display: String,
    expected: DesktopProbeExpectation,
    viewer_enabled: bool,
    viewer_required: bool,
}

impl DesktopProbeSpec {
    pub(crate) fn from_config(config: &Config) -> Result<Self, DesktopSupervisorError> {
        let width_px = u16::try_from(config.desktop().display_width())
            .map_err(|_| DesktopSupervisorError::InvalidConfiguration)?;
        let height_px = u16::try_from(config.desktop().display_height())
            .map_err(|_| DesktopSupervisorError::InvalidConfiguration)?;
        let display = std::env::var("DISPLAY")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ":99".to_owned());
        let viewer_required =
            parse_binary_setting(std::env::var("VIEWER_REQUIRED").ok().as_deref(), false)?;
        let viewer_enabled =
            parse_binary_setting(std::env::var("VIEWER_ENABLED").ok().as_deref(), true)?;
        if viewer_required && !viewer_enabled {
            return Err(DesktopSupervisorError::InvalidConfiguration);
        }
        Ok(Self {
            display,
            expected: DesktopProbeExpectation {
                width_px,
                height_px,
                depth: config.desktop().depth(),
                dpi: config.desktop().dpi(),
            },
            viewer_enabled,
            viewer_required,
        })
    }
}

fn parse_binary_setting(
    value: Option<&str>,
    default: bool,
) -> Result<bool, DesktopSupervisorError> {
    match value {
        None => Ok(default),
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(DesktopSupervisorError::InvalidConfiguration),
    }
}

/// Owned task and cancellation boundary for the desktop supervisor.
pub(crate) struct DesktopSupervisorHandle {
    cancellation: CancellationToken,
    join: JoinHandle<Result<(), DesktopSupervisorError>>,
}

impl DesktopSupervisorHandle {
    /// Returns a child-safe cancellation signal for the HTTP shutdown future.
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Request orderly actor cleanup and await the owned supervisor task.
    pub(crate) async fn shutdown(self) -> Result<(), DesktopSupervisorError> {
        self.cancellation.cancel();
        self.join
            .await
            .map_err(|_| DesktopSupervisorError::TaskPanicked)?
    }
}

/// Start probing without delaying the HTTP liveness listener.
pub(crate) fn spawn(
    readiness: ReadinessHandle,
    spec: DesktopProbeSpec,
    generation: DesktopGeneration,
) -> (
    DesktopSupervisorHandle,
    oneshot::Receiver<()>,
    watch::Receiver<Option<InputActorHandle>>,
) {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (fatal_tx, fatal_rx) = oneshot::channel();
    let (input_tx, input_rx) = watch::channel(None);
    let join = tokio::spawn(async move {
        run_supervisor(
            readiness,
            spec,
            generation,
            input_tx,
            task_cancellation,
            fatal_tx,
        )
        .await
    });
    (
        DesktopSupervisorHandle { cancellation, join },
        fatal_rx,
        input_rx,
    )
}

async fn run_supervisor(
    readiness: ReadinessHandle,
    spec: DesktopProbeSpec,
    generation: DesktopGeneration,
    input: watch::Sender<Option<InputActorHandle>>,
    cancellation: CancellationToken,
    fatal_tx: oneshot::Sender<()>,
) -> Result<(), DesktopSupervisorError> {
    readiness.transition_if_not_stopping(ReadinessSnapshot::new(
        DesktopReadiness::Probing,
        None,
        Some("desktop_capabilities_pending"),
    ));
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut runtime = loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match DesktopRuntime::start(&spec, startup_deadline).await {
            Ok(mut runtime) => {
                if cancellation.is_cancelled() {
                    runtime.shutdown().await?;
                    return Ok(());
                }
                break runtime;
            }
            Err(failure) if Instant::now() < startup_deadline => {
                tracing::debug!(probe = failure.code(), "desktop capability probe pending");
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(STARTUP_RETRY_INTERVAL) => {}
                }
            }
            Err(failure) => {
                tracing::error!(probe = failure.code(), "desktop capability startup failed");
                readiness.transition_if_not_stopping(ReadinessSnapshot::new(
                    DesktopReadiness::Failed,
                    None,
                    Some("desktop_capability_startup_failed"),
                ));
                let _ignored = fatal_tx.send(());
                return Err(DesktopSupervisorError::RequiredCapability(failure.code()));
            }
        }
    };

    input.send_replace(Some(runtime.input.clone()));
    publish_operational_readiness(
        &readiness,
        generation,
        spec.viewer_enabled,
        optional_viewer_is_available(&spec).await,
    );
    tracing::info!(desktop_generation = %generation, "desktop capabilities ready");

    let mut monitor = interval_at(Instant::now() + MONITOR_INTERVAL, MONITOR_INTERVAL);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => {
                input.send_replace(None);
                runtime.shutdown().await?;
                return Ok(());
            }
            _ = monitor.tick() => {
                if let Err(failure) = runtime.probe_required(&spec).await {
                    tracing::error!(probe = failure.code(), "required desktop capability was lost");
                    readiness.transition_if_not_stopping(ReadinessSnapshot::new(
                        DesktopReadiness::Failed,
                        Some(generation),
                        Some("desktop_capability_lost"),
                    ));
                    input.send_replace(None);
                    let cleanup = runtime.shutdown().await;
                    let _ignored = fatal_tx.send(());
                    cleanup?;
                    return Err(DesktopSupervisorError::RequiredCapability(failure.code()));
                }
                let viewer_available = probe_viewer_if_enabled(&spec).await.is_ok();
                if spec.viewer_required && !viewer_available {
                    let failure = ProbeFailure::Viewer;
                    tracing::error!(probe = failure.code(), "required desktop capability was lost");
                    readiness.transition_if_not_stopping(ReadinessSnapshot::new(
                        DesktopReadiness::Failed,
                        Some(generation),
                        Some("desktop_capability_lost"),
                    ));
                    input.send_replace(None);
                    let cleanup = runtime.shutdown().await;
                    let _ignored = fatal_tx.send(());
                    cleanup?;
                    return Err(DesktopSupervisorError::RequiredCapability(failure.code()));
                }
                publish_operational_readiness(
                    &readiness,
                    generation,
                    spec.viewer_enabled,
                    viewer_available,
                );
            }
        }
    }
}

struct DesktopRuntime {
    input: InputActorHandle,
    input_join: Option<InputActorJoin>,
}

impl DesktopRuntime {
    async fn start(
        spec: &DesktopProbeSpec,
        startup_deadline: Instant,
    ) -> Result<Self, ProbeFailure> {
        if startup_deadline.saturating_duration_since(Instant::now()) < BLOCKING_OPERATION_TIMEOUT {
            // Never launch a blocking startup worker unless its complete
            // containment budget remains. Expiring the larger startup budget
            // is an ordinary readiness failure, not a wedged worker.
            return Err(ProbeFailure::X11);
        }
        probe_x11(spec, startup_deadline, X11ProbeKind::StartupLifecycle).await?;
        let display = spec.display.clone();
        let input_start = tokio::task::spawn_blocking(move || spawn_input_actor(&display));
        let (input, input_join) = tokio::time::timeout_at(
            bounded_deadline(startup_deadline, BLOCKING_OPERATION_TIMEOUT),
            input_start,
        )
        .await
        .unwrap_or_else(|_| terminate_for_blocking_timeout("input_actor_start"))
        .map_err(|_| ProbeFailure::InputStart)?
        .map_err(|_| ProbeFailure::InputStart)?;
        let mut runtime = Self {
            input,
            input_join: Some(input_join),
        };
        let ready = async {
            probe_input(&runtime.input).await?;
            probe_atspi().await?;
            probe_viewer(spec).await
        }
        .await;
        if let Err(failure) = ready {
            let _cleanup = runtime.shutdown().await;
            return Err(failure);
        }
        Ok(runtime)
    }

    async fn probe_required(&self, spec: &DesktopProbeSpec) -> Result<(), ProbeFailure> {
        probe_x11(
            spec,
            Instant::now() + BLOCKING_OPERATION_TIMEOUT,
            X11ProbeKind::RecurringState,
        )
        .await?;
        probe_input(&self.input).await?;
        probe_atspi().await
    }

    async fn shutdown(&mut self) -> Result<(), DesktopSupervisorError> {
        let response_ok = matches!(
            timeout(OPERATION_TIMEOUT, self.input.shutdown()).await,
            Ok(Ok(Ok(ControlOutcome::Shutdown(evidence)))) if evidence.succeeded()
        );
        let join = self
            .input_join
            .take()
            .ok_or(DesktopSupervisorError::InputShutdown)?;
        let join_task = tokio::task::spawn_blocking(move || join.join());
        let exit = timeout(BLOCKING_OPERATION_TIMEOUT, join_task)
            .await
            .unwrap_or_else(|_| terminate_for_blocking_timeout("input_actor_join"))
            .map_err(|_| DesktopSupervisorError::TaskPanicked)?;
        if !response_ok || exit != InputActorExit::Stopped {
            return Err(DesktopSupervisorError::InputShutdown);
        }
        Ok(())
    }
}

async fn probe_x11(
    spec: &DesktopProbeSpec,
    terminal_deadline: Instant,
    kind: X11ProbeKind,
) -> Result<(), ProbeFailure> {
    let display = spec.display.clone();
    let expected = spec.expected;
    let probe = tokio::task::spawn_blocking(move || {
        let result = match kind {
            X11ProbeKind::StartupLifecycle => probe_desktop(&display, expected),
            X11ProbeKind::RecurringState => probe_desktop_steady_state(&display, expected),
        };
        if let Err(error) = &result {
            tracing::debug!(error = ?error, "X11 desktop probe was not ready");
        }
        result
    });
    tokio::time::timeout_at(
        bounded_deadline(terminal_deadline, BLOCKING_OPERATION_TIMEOUT),
        probe,
    )
    .await
    .unwrap_or_else(|_| terminate_for_blocking_timeout("x11_desktop_probe"))
    .map_err(|_| ProbeFailure::X11)?
    .map_err(|_| ProbeFailure::X11)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum X11ProbeKind {
    StartupLifecycle,
    RecurringState,
}

fn bounded_deadline(terminal: Instant, duration: Duration) -> Instant {
    terminal.min(Instant::now() + duration)
}

fn terminate_for_blocking_timeout(operation: &'static str) -> ! {
    tracing::error!(
        operation,
        "blocking desktop worker exceeded its terminal deadline"
    );
    // The worker cannot be detached and retried safely: it may still own an X11
    // connection or pressed-input cleanup. A non-zero process exit delegates
    // final containment to the critical s6 service and the container boundary.
    std::process::exit(TERMINAL_BLOCKING_EXIT_CODE)
}

async fn probe_input(input: &InputActorHandle) -> Result<(), ProbeFailure> {
    let response = timeout(OPERATION_TIMEOUT, input.probe())
        .await
        .map_err(|_| ProbeFailure::Input)?
        .map_err(|_| ProbeFailure::Input)?
        .map_err(|_| ProbeFailure::Input)?;
    match response {
        ControlOutcome::Probe(snapshot)
            if snapshot.thread == ActorThreadState::Running
                && snapshot.input == InputHealth::Healthy
                && snapshot.keyboard_model.availability == KeyboardModelAvailability::Available =>
        {
            Ok(())
        }
        ControlOutcome::Probe(_) | ControlOutcome::Reset(_) | ControlOutcome::Shutdown(_) => {
            Err(ProbeFailure::Input)
        }
    }
}

async fn probe_atspi() -> Result<(), ProbeFailure> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    let probe = LiveAtspiProbe::connect(deadline)
        .await
        .map_err(|_| ProbeFailure::Atspi)?;
    let _root_count = probe
        .probe_registry_service(deadline)
        .await
        .map_err(|_| ProbeFailure::Atspi)?;
    Ok(())
}

async fn probe_viewer(spec: &DesktopProbeSpec) -> Result<(), ProbeFailure> {
    if !spec.viewer_required {
        return Ok(());
    }
    probe_viewer_if_enabled(spec).await
}

async fn probe_viewer_if_enabled(spec: &DesktopProbeSpec) -> Result<(), ProbeFailure> {
    if !spec.viewer_enabled {
        return Ok(());
    }
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    probe_websocket_rfb(
        "127.0.0.1:6080".parse().map_err(|_| ProbeFailure::Viewer)?,
        deadline,
    )
    .await
}

async fn optional_viewer_is_available(spec: &DesktopProbeSpec) -> bool {
    !spec.viewer_enabled || spec.viewer_required || probe_viewer_if_enabled(spec).await.is_ok()
}

fn publish_operational_readiness(
    readiness: &ReadinessHandle,
    generation: DesktopGeneration,
    viewer_enabled: bool,
    viewer_available: bool,
) {
    let previous = readiness.snapshot();
    let snapshot = if viewer_enabled && !viewer_available {
        ReadinessSnapshot::new(
            DesktopReadiness::Degraded,
            Some(generation),
            Some("optional_viewer_unavailable"),
        )
    } else {
        ReadinessSnapshot::new(DesktopReadiness::Ready, Some(generation), None::<String>)
    };
    if previous != snapshot {
        if snapshot.state == DesktopReadiness::Degraded {
            tracing::warn!(
                capability = "viewer",
                reason_code = "optional_viewer_unavailable",
                "optional desktop capability is degraded"
            );
        } else if previous.state == DesktopReadiness::Degraded {
            tracing::info!(
                capability = "viewer",
                "optional desktop capability recovered"
            );
        }
        readiness.transition_if_not_stopping(snapshot);
    }
}

async fn probe_websocket_rfb(address: SocketAddr, deadline: Instant) -> Result<(), ProbeFailure> {
    const MAX_RESPONSE_HEADER_BYTES: usize = 4_096;
    const MAX_FRAME_PAYLOAD_BYTES: u64 = 1_048_576;
    const MAX_FRAMES: usize = 32;
    const MAX_DESKTOP_NAME_BYTES: usize = 4_096;

    let mut web = timeout_at_io(deadline, TcpStream::connect(address)).await?;
    timeout_at_io(
        deadline,
        web.write_all(
            b"GET /websockify HTTP/1.1\r\n\
Host: 127.0.0.1:6080\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
Sec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Protocol: binary\r\n\r\n",
        ),
    )
    .await?;

    let mut response_header = Vec::with_capacity(512);
    while !response_header.ends_with(b"\r\n\r\n") {
        if response_header.len() == MAX_RESPONSE_HEADER_BYTES {
            return Err(ProbeFailure::Viewer);
        }
        let mut byte = [0_u8; 1];
        timeout_at_io(deadline, web.read_exact(&mut byte)).await?;
        response_header.push(byte[0]);
    }
    if !valid_websocket_upgrade(&response_header) {
        return Err(ProbeFailure::Viewer);
    }

    let mut rfb_buffer = Vec::with_capacity(256);
    let mut fragmented = false;
    let mut frames_read = 0_usize;
    let greeting = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        12,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?;
    if greeting.as_slice() != b"RFB 003.008\n" {
        return Err(ProbeFailure::Viewer);
    }
    write_websocket_binary(&mut web, deadline, b"RFB 003.008\n").await?;

    let security_count = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        1,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?[0] as usize;
    if security_count == 0 || security_count > 32 {
        return Err(ProbeFailure::Viewer);
    }
    let security_types = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        security_count,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?;
    if !security_types.contains(&1) {
        return Err(ProbeFailure::Viewer);
    }
    write_websocket_binary(&mut web, deadline, &[1]).await?;

    let security_result = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        4,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?;
    if security_result.as_slice() != [0, 0, 0, 0] {
        return Err(ProbeFailure::Viewer);
    }
    write_websocket_binary(&mut web, deadline, &[1]).await?;

    let server_init = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        24,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?;
    let name_length = u32::from_be_bytes([
        server_init[20],
        server_init[21],
        server_init[22],
        server_init[23],
    ]);
    let name_length = usize::try_from(name_length).map_err(|_| ProbeFailure::Viewer)?;
    if name_length > MAX_DESKTOP_NAME_BYTES {
        return Err(ProbeFailure::Viewer);
    }
    let _desktop_name = read_rfb_bytes(
        &mut web,
        deadline,
        &mut rfb_buffer,
        &mut fragmented,
        &mut frames_read,
        name_length,
        MAX_FRAMES,
        MAX_FRAME_PAYLOAD_BYTES,
    )
    .await?;
    Ok(())
}

fn valid_websocket_upgrade(response: &[u8]) -> bool {
    const EXPECTED_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    let Ok(text) = std::str::from_utf8(response) else {
        return false;
    };
    let mut lines = text.trim_end_matches("\r\n").split("\r\n");
    if !lines
        .next()
        .is_some_and(|status| status.starts_with("HTTP/1.1 101 "))
    {
        return false;
    }
    let mut upgrade = None;
    let mut connection = None;
    let mut accept = None;
    let mut protocol = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        let slot = match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => &mut upgrade,
            "connection" => &mut connection,
            "sec-websocket-accept" => &mut accept,
            "sec-websocket-protocol" => &mut protocol,
            _ => continue,
        };
        if slot.replace(value.trim()).is_some() {
            return false;
        }
    }
    upgrade.is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && connection.is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        && accept == Some(EXPECTED_ACCEPT)
        && protocol == Some("binary")
}

#[allow(clippy::too_many_arguments)]
async fn read_rfb_bytes(
    web: &mut TcpStream,
    deadline: Instant,
    buffered: &mut Vec<u8>,
    fragmented: &mut bool,
    frames_read: &mut usize,
    length: usize,
    max_frames: usize,
    max_frame_payload_bytes: u64,
) -> Result<Vec<u8>, ProbeFailure> {
    while buffered.len() < length {
        *frames_read = frames_read.checked_add(1).ok_or(ProbeFailure::Viewer)?;
        if *frames_read > max_frames {
            return Err(ProbeFailure::Viewer);
        }
        let mut frame_header = [0_u8; 2];
        timeout_at_io(deadline, web.read_exact(&mut frame_header)).await?;
        if frame_header[0] & 0x70 != 0 || frame_header[1] & 0x80 != 0 {
            return Err(ProbeFailure::Viewer);
        }
        let opcode = frame_header[0] & 0x0f;
        let payload_length = match frame_header[1] & 0x7f {
            length @ 0..=125 => u64::from(length),
            126 => {
                let mut bytes = [0_u8; 2];
                timeout_at_io(deadline, web.read_exact(&mut bytes)).await?;
                u64::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0_u8; 8];
                timeout_at_io(deadline, web.read_exact(&mut bytes)).await?;
                u64::from_be_bytes(bytes)
            }
            _ => return Err(ProbeFailure::Viewer),
        };
        if payload_length > max_frame_payload_bytes {
            return Err(ProbeFailure::Viewer);
        }
        let final_frame = frame_header[0] & 0x80 != 0;
        match opcode {
            0x2 if !*fragmented => *fragmented = !final_frame,
            0x0 if *fragmented => *fragmented = !final_frame,
            0x9 | 0xA if final_frame && payload_length <= 125 => {}
            _ => return Err(ProbeFailure::Viewer),
        }

        let payload_length = usize::try_from(payload_length).map_err(|_| ProbeFailure::Viewer)?;
        let mut payload = vec![0_u8; payload_length];
        timeout_at_io(deadline, web.read_exact(&mut payload)).await?;
        if opcode == 0x9 {
            write_websocket_frame(web, deadline, 0xA, &payload).await?;
        } else if matches!(opcode, 0x0 | 0x2) {
            if buffered.len().saturating_add(payload.len())
                > usize::try_from(max_frame_payload_bytes).map_err(|_| ProbeFailure::Viewer)?
            {
                return Err(ProbeFailure::Viewer);
            }
            buffered.extend_from_slice(&payload);
        }
    }

    Ok(buffered.drain(..length).collect())
}

async fn write_websocket_binary(
    web: &mut TcpStream,
    deadline: Instant,
    payload: &[u8],
) -> Result<(), ProbeFailure> {
    write_websocket_frame(web, deadline, 0x2, payload).await
}

async fn write_websocket_frame(
    web: &mut TcpStream,
    deadline: Instant,
    opcode: u8,
    payload: &[u8],
) -> Result<(), ProbeFailure> {
    let length = u8::try_from(payload.len()).map_err(|_| ProbeFailure::Viewer)?;
    if length > 125 || !matches!(opcode, 0x2 | 0xA) {
        return Err(ProbeFailure::Viewer);
    }
    let random = uuid::Uuid::new_v4();
    let mask = &random.as_bytes()[..4];
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[0x80 | opcode, 0x80 | length]);
    frame.extend_from_slice(mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    timeout_at_io(deadline, web.write_all(&frame)).await
}

async fn timeout_at_io<T>(
    deadline: Instant,
    future: impl Future<Output = std::io::Result<T>>,
) -> Result<T, ProbeFailure> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| ProbeFailure::Viewer)?
        .map_err(|_| ProbeFailure::Viewer)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeFailure {
    X11,
    InputStart,
    Input,
    Atspi,
    Viewer,
}

impl ProbeFailure {
    const fn code(self) -> &'static str {
        match self {
            Self::X11 => "x11_desktop",
            Self::InputStart => "input_actor_start",
            Self::Input => "input_actor_probe",
            Self::Atspi => "atspi_registry",
            Self::Viewer => "viewer_gateway",
        }
    }
}

/// Supervisor startup, owned task, or cleanup failure.
#[derive(Debug, Error)]
pub(crate) enum DesktopSupervisorError {
    /// Validated configuration could not form the fixed runtime probe spec.
    #[error("desktop probe configuration is invalid")]
    InvalidConfiguration,
    /// A required capability did not become or remain ready.
    #[error("required desktop capability failed: {0}")]
    RequiredCapability(&'static str),
    /// The input actor could not prove orderly cleanup.
    #[error("input actor shutdown cleanup failed")]
    InputShutdown,
    /// An owned blocking or async task panicked.
    #[error("desktop supervisor task panicked")]
    TaskPanicked,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        time::Instant,
    };
    use xenoteer_protocol::DesktopGeneration;
    use xenoteer_server::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot};

    use super::{
        DesktopProbeExpectation, DesktopProbeSpec, DesktopRuntime, DesktopSupervisorError,
        ProbeFailure, parse_binary_setting, probe_websocket_rfb, publish_operational_readiness,
        spawn, valid_websocket_upgrade,
    };

    async fn read_client_binary(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).await?;
        if header[0] != 0x82 || header[1] & 0x80 == 0 || header[1] & 0x7f > 125 {
            return Err(std::io::Error::other("invalid client WebSocket frame"));
        }
        let length = usize::from(header[1] & 0x7f);
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask).await?;
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
        Ok(payload)
    }

    async fn write_server_binary(
        stream: &mut tokio::net::TcpStream,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let length = u8::try_from(payload.len())
            .map_err(|_| std::io::Error::other("test payload length overflow"))?;
        if length > 125 {
            return Err(std::io::Error::other("test payload exceeds short frame"));
        }
        stream.write_all(&[0x82, length]).await?;
        stream.write_all(payload).await
    }

    #[test]
    fn failure_codes_are_closed_and_non_secret() {
        assert_eq!(ProbeFailure::X11.code(), "x11_desktop");
        assert_eq!(ProbeFailure::InputStart.code(), "input_actor_start");
        assert_eq!(ProbeFailure::Input.code(), "input_actor_probe");
        assert_eq!(ProbeFailure::Atspi.code(), "atspi_registry");
        assert_eq!(ProbeFailure::Viewer.code(), "viewer_gateway");
    }

    #[test]
    fn viewer_settings_are_closed_binary_values() {
        assert!(matches!(parse_binary_setting(None, true), Ok(true)));
        assert!(matches!(parse_binary_setting(Some("0"), true), Ok(false)));
        assert!(matches!(parse_binary_setting(Some("1"), false), Ok(true)));
        assert!(matches!(
            parse_binary_setting(Some("true"), false),
            Err(DesktopSupervisorError::InvalidConfiguration)
        ));
    }

    #[test]
    fn viewer_upgrade_requires_all_security_relevant_headers() {
        let valid = b"HTTP/1.1 101 Switching Protocols\r\n\
Upgrade: websocket\r\n\
Connection: keep-alive, Upgrade\r\n\
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
Sec-WebSocket-Protocol: binary\r\n\r\n";
        assert!(valid_websocket_upgrade(valid));
        assert!(!valid_websocket_upgrade(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n"
        ));
    }

    #[test]
    fn optional_viewer_degrades_and_recovers_without_losing_readiness() {
        let generation = xenoteer_protocol::DesktopGeneration::new();
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Booting,
            None,
            Some("test_boot"),
        ));
        publish_operational_readiness(&readiness, generation, true, false);
        let degraded = readiness.snapshot();
        assert_eq!(degraded.state, DesktopReadiness::Degraded);
        assert_eq!(
            degraded.reason_code.as_deref(),
            Some("optional_viewer_unavailable")
        );
        assert!(degraded.is_ready());

        publish_operational_readiness(&readiness, generation, true, true);
        assert_eq!(readiness.snapshot().state, DesktopReadiness::Ready);

        publish_operational_readiness(&readiness, generation, false, false);
        assert_eq!(readiness.snapshot().state, DesktopReadiness::Ready);
    }

    #[tokio::test]
    async fn expired_startup_budget_does_not_launch_a_blocking_x11_worker() {
        let spec = DesktopProbeSpec {
            display: ":65534".to_owned(),
            expected: DesktopProbeExpectation {
                width_px: 1_920,
                height_px: 1_080,
                depth: 24,
                dpi: 96,
            },
            viewer_enabled: false,
            viewer_required: false,
        };
        assert!(matches!(
            DesktopRuntime::start(&spec, Instant::now()).await,
            Err(ProbeFailure::X11)
        ));
    }

    #[tokio::test]
    async fn viewer_probe_accepts_fragmented_rfb_websocket_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::with_capacity(512);
            while !request.ends_with(b"\r\n\r\n") {
                if request.len() == 4_096 {
                    return Err(std::io::Error::other("request header exceeded test bound"));
                }
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).await?;
                request.push(byte[0]);
            }
            if !request.starts_with(b"GET /websockify HTTP/1.1\r\n") {
                return Err(std::io::Error::other("unexpected WebSocket request"));
            }
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
Sec-WebSocket-Protocol: binary\r\n\r\n\
\x02\x04RFB \x80\x08003.008\n",
                )
                .await?;
            if read_client_binary(&mut stream).await? != b"RFB 003.008\n" {
                return Err(std::io::Error::other("unexpected RFB client version"));
            }
            write_server_binary(&mut stream, &[1, 1]).await?;
            if read_client_binary(&mut stream).await? != [1] {
                return Err(std::io::Error::other("unexpected RFB security choice"));
            }
            write_server_binary(&mut stream, &[0, 0, 0, 0]).await?;
            if read_client_binary(&mut stream).await? != [1] {
                return Err(std::io::Error::other("unexpected RFB ClientInit"));
            }
            let desktop_name = b"xenoteer-test";
            let mut server_init = Vec::with_capacity(24 + desktop_name.len());
            server_init.extend_from_slice(&1_920_u16.to_be_bytes());
            server_init.extend_from_slice(&1_080_u16.to_be_bytes());
            server_init.extend_from_slice(&[0_u8; 16]);
            server_init.extend_from_slice(
                &u32::try_from(desktop_name.len())
                    .map_err(|_| std::io::Error::other("test name length overflow"))?
                    .to_be_bytes(),
            );
            server_init.extend_from_slice(desktop_name);
            write_server_binary(&mut stream, &server_init).await?;
            Ok::<(), std::io::Error>(())
        });

        assert!(
            probe_websocket_rfb(address, Instant::now() + Duration::from_secs(2))
                .await
                .is_ok()
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_joins_a_not_ready_supervisor_without_signalling_fatal()
    -> Result<(), Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Booting,
            None,
            Some("test_startup"),
        ));
        let spec = DesktopProbeSpec {
            display: ":65534".to_owned(),
            expected: DesktopProbeExpectation {
                width_px: 1_920,
                height_px: 1_080,
                depth: 24,
                dpi: 96,
            },
            viewer_enabled: false,
            viewer_required: false,
        };
        let (handle, fatal, input) = spawn(readiness.clone(), spec, DesktopGeneration::new());
        assert!(input.borrow().is_none());
        handle.cancellation().cancel();
        readiness.transition(ReadinessSnapshot::new(
            DesktopReadiness::Draining,
            None,
            Some("test_shutdown"),
        ));
        handle.shutdown().await?;
        assert_eq!(readiness.snapshot().state, DesktopReadiness::Draining);
        assert!(fatal.await.is_err());
        Ok(())
    }
}
