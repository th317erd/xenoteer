//! Xenoteer daemon composition entry point.

#![forbid(unsafe_code)]

mod accessibility_events;
mod accessibility_plane;
mod accessibility_runtime;
#[cfg(test)]
mod accessibility_runtime_tests;
mod artifact_service;
mod capability_monitor;
mod clipboard_events;
mod clipboard_service;
mod control_plane;
mod desktop_supervisor;
mod event_sink;
mod observation_plane;
#[allow(dead_code)] // Phase 3 routes wire this verified adapter in the next integration step.
mod process_manager;
mod runtime_capabilities;
mod screenshot_service;
mod semantic_actions;
mod shutdown;

use std::{fs, net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use xenoteer_artifacts::{ArtifactLimits, ArtifactStore, StoreError};
use xenoteer_core::{Config, ConfigLoadError, ConfigOverrides, ViewerConfig, WindowModelLimits};
use xenoteer_processd::{BrokerClient, DEFAULT_BROKER_SOCKET};
use xenoteer_protocol::{DesktopGeneration, DesktopId};
use xenoteer_server::{
    AllowedOrigins, ApiServices, Authentication, DesktopReadiness, Grant,
    InMemoryViewerTicketRegistry, LoopbackWebsockifyConnector, OriginPolicyError, Principal,
    PrincipalError, ReadinessHandle, ReadinessSnapshot, StaticTokenProvider, TokenLoadError,
    TransportLimitError, TransportLimits, ViewerGateway, ViewerGatewayConfigurationError,
    ViewerGatewayLimits, ViewerTicketRegistryConfig, ViewerTicketRegistryError,
    api_router_with_services, serve,
};
use xenoteer_x11::capture::{
    CaptureActorExit, CaptureActorHandle, CaptureActorState, spawn_capture_actor,
};
use xenoteer_x11::{
    ClipboardActorExit, ClipboardActorHandle, ClipboardActorState, WindowControlActorExit,
    WindowControlActorHandle, WindowControlActorState, X11Error, spawn_clipboard_actor,
    spawn_window_control_actor,
};

use crate::{
    accessibility_runtime::{AccessibilityRuntimeError, spawn_live_accessibility_runtime},
    artifact_service::{
        ArtifactRetentionPolicy, ArtifactUploadTimeoutPolicy, RetentionPolicyError,
        StoreArtifactService, UploadTimeoutPolicyError,
    },
    capability_monitor::{
        OperationBackendMonitorError, WindowCapabilityMonitorError,
        spawn_operation_backend_monitor, spawn_window_capability_monitor,
    },
    clipboard_events::{ClipboardEventRelayError, spawn_clipboard_event_relay},
    clipboard_service::DaemonClipboardReadService,
    event_sink::{DeferredEventSinkBindError, DeferredWindowEventSink},
    observation_plane::{
        ObservationCompositionError, ObservationServiceExit, ObservationServiceSettings,
        WindowEventSink, spawn_live_observation_service_with_broker_and_event_sink,
    },
    runtime_capabilities::{
        RuntimeCapabilityBackends, RuntimeCapabilityError, RuntimeCapabilityProvider,
    },
    screenshot_service::DaemonScreenshotService,
};

const CONFIG_PATH_ENV: &str = "XENOTEER_CONFIG";

#[derive(Debug, Parser)]
#[command(
    name = "xenoteerd",
    version,
    about = "Xenoteer X11 desktop-control daemon"
)]
struct Args {
    /// TOML configuration file.
    #[arg(long, env = "XENOTEER_CONFIG")]
    config: Option<PathBuf>,
    /// Final-precedence API bind address.
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Disable authentication for loopback-only development.
    #[arg(long)]
    insecure_disable_auth: bool,
    /// Final-precedence tracing filter.
    #[arg(long)]
    log_filter: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xenoteerd startup failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), DaemonError> {
    let args = Args::parse();
    let file = match &args.config {
        Some(path) => Some(fs::read_to_string(path).map_err(DaemonError::ReadConfig)?),
        None => None,
    };
    let mut overrides = ConfigOverrides::default();
    if let Some(listen) = args.listen {
        overrides = overrides.with_listen(listen);
    }
    if args.insecure_disable_auth {
        overrides = overrides.with_insecure_disable_auth(true);
    }
    if let Some(filter) = args.log_filter {
        overrides = overrides.with_log_filter(filter);
    }
    let config = Config::load(
        file.as_deref(),
        configuration_environment(std::env::vars()),
        &overrides,
    )?;

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_new(config.logging().filter()).map_err(DaemonError::LogFilter)?,
        )
        .with_current_span(true)
        .with_span_list(true)
        .try_init()
        .map_err(|error| DaemonError::Tracing(error.to_string()))?;
    tracing::info!(config = ?config.redacted_summary(), "loaded validated configuration");

    let operator = configured_principal(&config)?;
    let authentication = if config.server().insecure_disable_auth() {
        Authentication::insecure_development(operator)
    } else {
        Authentication::bearer(StaticTokenProvider::from_file(
            config.auth().token_file().expose_path(),
            operator,
        )?)
    };
    let transport_limits = TransportLimits::default().with_max_body_bytes(
        usize::try_from(config.server().request_body_limit_bytes())
            .map_err(|_| TransportLimitError::BodyBytes)?,
    )?;
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();

    let viewer_config = config.viewer().clone();
    let viewer = tokio::task::spawn_blocking(move || configured_viewer(&viewer_config))
        .await
        .map_err(|error| DaemonError::StartupTask(error.to_string()))??;
    let viewer_enabled = viewer.enabled();

    let artifact_limits = configured_artifact_limits(&config)?;
    let artifact_root = config.artifacts().root_directory().to_owned();
    let artifact_store =
        tokio::task::spawn_blocking(move || ArtifactStore::open(artifact_root, artifact_limits))
            .await
            .map_err(|error| DaemonError::StartupTask(error.to_string()))??;
    let artifact_service = Arc::new(StoreArtifactService::new(
        Arc::new(artifact_store),
        ArtifactRetentionPolicy::new(Duration::from_millis(
            config.artifacts().clipboard_input_retention_ms(),
        ))?
        .with_generated_retention(Duration::from_millis(config.artifacts().max_retention_ms()))?,
        ArtifactUploadTimeoutPolicy::new(
            Duration::from_millis(config.artifacts().upload_total_timeout_ms()),
            Duration::from_millis(config.artifacts().upload_idle_timeout_ms()),
        )?,
    ));

    let observation_settings = configured_observation_settings(&config)?;
    let observation_limits = configured_window_model_limits(&config);
    let window_event_sink = Arc::new(DeferredWindowEventSink::new());
    let observation_event_sink: Arc<dyn WindowEventSink> = window_event_sink.clone();
    let display = configured_display();
    let clipboard_display = display.clone();
    let capture_display = display.clone();
    let window_control_display = display.clone();
    let (observation_service, observation_shutdown, observation_join) =
        tokio::task::spawn_blocking(move || {
            spawn_live_observation_service_with_broker_and_event_sink(
                &display,
                desktop_id,
                desktop_generation,
                observation_limits,
                observation_settings,
                BrokerClient::new(DEFAULT_BROKER_SOCKET),
                observation_event_sink,
            )
        })
        .await
        .map_err(|error| DaemonError::StartupTask(error.to_string()))??;
    let observation_join = DetachedJoinOwner::new("xenoteer-observation-join-monitor", move || {
        observation_join.join()
    });
    let (clipboard_handle, clipboard_events, clipboard_join) =
        tokio::task::spawn_blocking(move || spawn_clipboard_actor(&clipboard_display))
            .await
            .map_err(|error| DaemonError::StartupTask(error.to_string()))?
            .map_err(DaemonError::ClipboardStartup)?;
    let clipboard_join = DetachedJoinOwner::new("xenoteer-clipboard-join-monitor", move || {
        clipboard_join.join()
    });
    let (capture_handle, capture_join) =
        tokio::task::spawn_blocking(move || spawn_capture_actor(&capture_display))
            .await
            .map_err(|error| DaemonError::StartupTask(error.to_string()))?
            .map_err(DaemonError::CaptureStartup)?;
    let capture_join =
        DetachedJoinOwner::new("xenoteer-capture-join-monitor", move || capture_join.join());
    let (window_control_handle, window_control_join) =
        tokio::task::spawn_blocking(move || spawn_window_control_actor(&window_control_display))
            .await
            .map_err(|error| DaemonError::StartupTask(error.to_string()))?
            .map_err(DaemonError::WindowControlStartup)?;
    let window_control_join =
        DetachedJoinOwner::new("xenoteer-window-control-join-monitor", move || {
            window_control_join.join()
        });
    let (window_capability_monitor, window_capability_reader) =
        spawn_window_capability_monitor(window_control_handle.clone());
    let (operation_backend_monitor, operation_backend_reader) = spawn_operation_backend_monitor(
        Arc::clone(&artifact_service),
        BrokerClient::new(DEFAULT_BROKER_SOCKET),
    );
    let accessibility_event_sink: Arc<dyn WindowEventSink> = window_event_sink.clone();
    let accessibility_runtime = spawn_live_accessibility_runtime(
        config.accessibility(),
        desktop_id,
        desktop_generation,
        accessibility_event_sink,
    )?;
    let accessibility_plane = accessibility_runtime.plane();
    let accessibility_reader = accessibility_runtime.reader();

    let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
        DesktopReadiness::Booting,
        None,
        Some("startup_in_progress"),
    ));
    let shutdown_signals = shutdown::ShutdownSignals::install()?;
    let listener = TcpListener::bind(config.server().listen())
        .await
        .map_err(DaemonError::Bind)?;
    let local_address = listener.local_addr().map_err(DaemonError::Bind)?;

    let desktop_spec = desktop_supervisor::DesktopProbeSpec::from_config(&config)?;
    let (desktop_supervisor, mut desktop_fatal, desktop_input) =
        desktop_supervisor::spawn(readiness.clone(), desktop_spec, desktop_generation);
    let capabilities = RuntimeCapabilityProvider::new(
        readiness.clone(),
        viewer_enabled,
        desktop_input.clone(),
        RuntimeCapabilityBackends::new(
            Arc::clone(&observation_service),
            capture_handle.clone(),
            clipboard_handle.clone(),
            window_control_handle.clone(),
            operation_backend_reader,
            window_capability_reader,
            accessibility_reader,
        ),
    )?;
    let window_control_runtime = control_plane::WindowControlRuntime::new(
        window_control_handle.clone(),
        Arc::clone(&observation_service),
    );
    let semantic_runtime = accessibility_runtime.semantic_runtime();
    let clipboard_runtime = control_plane::ClipboardRuntime::new(
        clipboard_handle.clone(),
        Arc::clone(&artifact_service),
        Arc::clone(&observation_service),
        window_control_runtime,
        desktop_input,
        Some(semantic_runtime.clone()),
        desktop_id,
        desktop_generation,
    );
    let coordinator = control_plane::spawn_with_accessibility_runtime(
        &config,
        desktop_id,
        desktop_generation,
        clipboard_runtime,
        semantic_runtime,
    )?;
    window_event_sink.bind(coordinator.event_ingress())?;
    let clipboard_event_relay = spawn_clipboard_event_relay(
        clipboard_events,
        window_event_sink.clone(),
        desktop_id,
        desktop_generation,
    );
    let coordinator_shutdown = coordinator.shutdown_handle();
    tracing::info!(
        listen = %local_address,
        readiness = "probing",
        reason_code = "desktop_capabilities_pending",
        "xenoteerd HTTP listener started"
    );

    let shutdown_readiness = readiness.clone();
    let screenshot_service = Arc::new(DaemonScreenshotService::new(
        capture_handle.clone(),
        Arc::clone(&observation_service),
        Arc::clone(&artifact_service),
    ));
    let clipboard_read_service = Arc::new(DaemonClipboardReadService::new(
        clipboard_handle.clone(),
        Arc::clone(&artifact_service),
        desktop_id,
        desktop_generation,
    ));
    let services = ApiServices::new(coordinator.control(), observation_service)
        .with_accessibility_plane(accessibility_plane)
        .with_artifact_service(artifact_service)
        .with_clipboard_read_service(clipboard_read_service)
        .with_screenshot_service(screenshot_service);
    let (origins, services) = viewer.into_router_parts(services);
    let application = api_router_with_services(
        readiness.clone(),
        desktop_id,
        authentication,
        capabilities,
        transport_limits,
        origins,
        services,
    );
    // Keep the RAII join owner local through every fallible startup step. Once
    // composition is complete, a blocking monitor can safely own it and feed
    // terminal failure into graceful HTTP shutdown.
    let observation_monitor = observation_join.into_monitor()?;
    let mut observation_fatal = observation_monitor.clone();
    let signal_observation_shutdown = observation_shutdown.clone();
    let signal_window_control_shutdown = window_control_handle.clone();
    let signal_capture_shutdown = capture_handle.clone();
    let signal_clipboard_shutdown = clipboard_handle.clone();
    let signal_accessibility_shutdown = accessibility_runtime.shutdown_handle();
    let monitor_window_control = window_control_handle.clone();
    let monitor_capture = capture_handle.clone();
    let monitor_clipboard = clipboard_handle.clone();
    let serve_result = serve(listener, application, async move {
        tokio::select! {
            signal = shutdown_signals.wait() => {
                match signal {
                    Ok(signal) => tracing::info!(signal = %signal, "shutdown signal received"),
                    Err(error) => {
                        tracing::error!(error = %error, "failed to listen for shutdown signal");
                    }
                }
            }
            _ = &mut desktop_fatal => {
                tracing::error!("desktop supervisor requested daemon shutdown");
            }
            exit = observation_fatal.wait() => {
                tracing::error!(?exit, "observation service requested daemon shutdown");
            }
            state = wait_for_window_control_failure(monitor_window_control) => {
                tracing::error!(?state, "window-control actor requested daemon shutdown");
            }
            state = wait_for_capture_failure(monitor_capture) => {
                tracing::error!(?state, "capture actor requested daemon shutdown");
            }
            state = wait_for_clipboard_failure(monitor_clipboard) => {
                tracing::error!(?state, "clipboard actor requested daemon shutdown");
            }
        }
        shutdown_readiness.transition(ReadinessSnapshot::new(
            DesktopReadiness::Draining,
            Some(desktop_generation),
            Some("shutdown_in_progress"),
        ));
        if let Err(error) = coordinator_shutdown.shutdown().await
            && error != xenoteer_core::coordinator::CoordinatorError::Closed
        {
            tracing::error!(%error, "coordinator shutdown failed during HTTP drain");
        }
        let _ = signal_window_control_shutdown.shutdown();
        let _ = signal_capture_shutdown.shutdown();
        let _ = signal_clipboard_shutdown.shutdown();
        signal_accessibility_shutdown.request();
        signal_observation_shutdown.request();
    })
    .await;

    let window_capability_monitor_result = window_capability_monitor.shutdown().await;
    let operation_backend_monitor_result = operation_backend_monitor.shutdown().await;
    let clipboard_event_result = clipboard_event_relay.shutdown().await;
    let coordinator_result = coordinator.shutdown().await;
    let accessibility_result = accessibility_runtime.shutdown().await;
    let _ = window_control_handle.shutdown();
    let _ = capture_handle.shutdown();
    let _ = clipboard_handle.shutdown();
    let window_control_monitor = window_control_join.into_monitor();
    let capture_monitor = capture_join.into_monitor();
    let clipboard_monitor = clipboard_join.into_monitor();
    let (window_control_first, capture_first, clipboard_first) = tokio::join!(
        first_join_attempt(window_control_monitor, Duration::from_secs(5)),
        first_join_attempt(capture_monitor, Duration::from_secs(5)),
        first_join_attempt(clipboard_monitor, Duration::from_secs(5)),
    );
    observation_shutdown.request();
    let observation_first =
        first_join_attempt(Ok(observation_monitor), Duration::from_secs(5)).await;
    // Preserve the X server until window control, capture, clipboard, and
    // observation have had their ordinary shutdown window. Supervisor teardown
    // remains unconditional if an actor needs the bounded second join attempt.
    let supervisor_result = desktop_supervisor.shutdown().await;
    let (window_control_result, capture_result, clipboard_result, observation_result) = tokio::join!(
        finish_join_attempt(window_control_first, Duration::from_secs(2)),
        finish_join_attempt(capture_first, Duration::from_secs(2)),
        finish_join_attempt(clipboard_first, Duration::from_secs(2)),
        finish_join_attempt(observation_first, Duration::from_secs(2)),
    );
    let window_control_result = window_control_result.map_err(|error| match error {
        ActorJoinWaitError::Monitor(error) => DaemonError::ActorJoinMonitor(error),
        ActorJoinWaitError::TimedOut => DaemonError::WindowControlShutdownTimeout,
    });
    let capture_result = capture_result.map_err(|error| match error {
        ActorJoinWaitError::Monitor(error) => DaemonError::ActorJoinMonitor(error),
        ActorJoinWaitError::TimedOut => DaemonError::CaptureShutdownTimeout,
    });
    let clipboard_result = clipboard_result.map_err(|error| match error {
        ActorJoinWaitError::Monitor(error) => DaemonError::ActorJoinMonitor(error),
        ActorJoinWaitError::TimedOut => DaemonError::ClipboardShutdownTimeout,
    });
    let observation_result = observation_result.map_err(|error| match error {
        ActorJoinWaitError::Monitor(error) => DaemonError::ActorJoinMonitor(error),
        ActorJoinWaitError::TimedOut => DaemonError::ObservationShutdownTimeout,
    });
    if let Err(error) = serve_result {
        return Err(DaemonError::Serve(error));
    }
    coordinator_result?;
    clipboard_event_result?;
    window_capability_monitor_result?;
    operation_backend_monitor_result?;
    supervisor_result?;
    let window_control_result = window_control_result?;
    let capture_result = capture_result?;
    let clipboard_result = clipboard_result?;
    let observation_result = observation_result?;
    if window_control_result != WindowControlActorExit::Stopped {
        return Err(DaemonError::WindowControlRuntime(window_control_result));
    }
    if observation_result != ObservationServiceExit::Stopped {
        return Err(DaemonError::ObservationRuntime(observation_result));
    }
    if capture_result != CaptureActorExit::Stopped {
        return Err(DaemonError::CaptureRuntime(capture_result));
    }
    if clipboard_result != ClipboardActorExit::Stopped {
        return Err(DaemonError::ClipboardRuntime(clipboard_result));
    }
    readiness.transition(ReadinessSnapshot::new(
        DesktopReadiness::Stopped,
        None,
        Some("shutdown_complete"),
    ));
    tracing::info!("clipboard actor stopped");
    tracing::info!("capture actor stopped");
    if accessibility_result.actor_exit != Some(xenoteer_atspi::AtspiActorExit::Stopped)
        || !accessibility_result.mirror_stopped
    {
        tracing::warn!(
            ?accessibility_result,
            "accessibility runtime stopped incompletely"
        );
    } else {
        tracing::info!("accessibility runtime stopped");
    }
    tracing::info!("xenoteerd stopped");
    Ok(())
}

fn configured_principal(config: &Config) -> Result<Principal, DaemonError> {
    let grants = config
        .auth()
        .grants()
        .iter()
        .map(|name| Grant::from_name(name).ok_or(DaemonError::AuthorizationGrantInvariant))
        .collect::<Result<Vec<_>, _>>()?;
    Principal::new("local-operator", grants).map_err(Into::into)
}

fn configured_artifact_limits(config: &Config) -> Result<ArtifactLimits, DaemonError> {
    let artifacts = config.artifacts();
    Ok(ArtifactLimits::new(
        artifacts.max_object_bytes(),
        artifacts.max_total_bytes(),
        artifacts.max_objects(),
        artifacts.max_owner_bytes(),
        artifacts.max_owner_objects(),
        artifacts.max_retention_ms(),
    )?)
}

fn configured_observation_settings(
    config: &Config,
) -> Result<ObservationServiceSettings, DaemonError> {
    let observation = config.observation();
    Ok(ObservationServiceSettings::new(
        observation.request_capacity(),
        observation.max_waiters(),
        observation.token_capacity(),
        observation.cursor_ttl_ms(),
        observation.reference_ttl_ms(),
        Duration::from_millis(observation.raw_request_timeout_ms()),
        Duration::from_millis(observation.startup_timeout_ms()),
        Duration::from_millis(observation.idle_poll_interval_ms()),
    )?)
}

fn configured_window_model_limits(config: &Config) -> WindowModelLimits {
    let observation = config.observation();
    WindowModelLimits {
        max_live_windows: observation.max_live_windows(),
        max_tombstones: observation.max_tombstones(),
        tombstone_ttl_ms: observation.tombstone_ttl_ms(),
    }
}

fn configured_display() -> String {
    std::env::var("DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ":99".to_owned())
}

async fn wait_for_window_control_failure(
    handle: WindowControlActorHandle,
) -> WindowControlActorState {
    loop {
        let state = handle.health().state;
        if !matches!(
            state,
            WindowControlActorState::Starting | WindowControlActorState::Healthy
        ) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_capture_failure(handle: CaptureActorHandle) -> CaptureActorState {
    loop {
        let state = handle.health().state;
        if !matches!(
            state,
            CaptureActorState::Starting | CaptureActorState::Healthy
        ) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_clipboard_failure(handle: ClipboardActorHandle) -> ClipboardActorState {
    loop {
        let state = handle.health().state;
        if !matches!(
            state,
            ClipboardActorState::Starting | ClipboardActorState::Healthy
        ) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Clone)]
struct DetachedJoinMonitor<T> {
    exit: tokio::sync::watch::Receiver<Option<T>>,
}

impl<T: Copy> DetachedJoinMonitor<T> {
    async fn wait(&mut self) -> Result<T, DetachedJoinMonitorError> {
        loop {
            let current = *self.exit.borrow_and_update();
            if let Some(exit) = current {
                return Ok(exit);
            }
            self.exit
                .changed()
                .await
                .map_err(|_| DetachedJoinMonitorError::Closed)?;
        }
    }
}

fn spawn_detached_join_monitor<T, F>(
    name: &'static str,
    join: F,
) -> Result<DetachedJoinMonitor<T>, DetachedJoinMonitorError>
where
    T: Copy + Send + Sync + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (exit_tx, exit) = tokio::sync::watch::channel(None);
    let (join_tx, join_rx) = std::sync::mpsc::sync_channel::<F>(1);
    let thread = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let Ok(join) = join_rx.recv() else {
                return;
            };
            let exit = join();
            let _ = exit_tx.send(Some(exit));
        });
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            // A join owner's Drop may itself join indefinitely. Retain it until
            // the process exits rather than moving that unbounded wait back
            // onto Tokio's runtime thread when the OS cannot create a monitor.
            std::mem::forget(join);
            return Err(DetachedJoinMonitorError::Spawn(error));
        }
    };
    if let Err(std::sync::mpsc::SendError(join)) = join_tx.send(join) {
        // The monitor closed before accepting ownership. Preserve the same
        // bounded-failure guarantee as the thread-creation error above.
        std::mem::forget(join);
        return Err(DetachedJoinMonitorError::Closed);
    }
    drop(thread);
    Ok(DetachedJoinMonitor { exit })
}

/// Owns an actor join closure without permitting synchronous cleanup on a
/// Tokio runtime thread when later daemon composition fails.
struct DetachedJoinOwner<T, F>
where
    T: Copy + Send + Sync + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    monitor_name: &'static str,
    join: Option<F>,
}

impl<T, F> DetachedJoinOwner<T, F>
where
    T: Copy + Send + Sync + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    const fn new(monitor_name: &'static str, join: F) -> Self {
        Self {
            monitor_name,
            join: Some(join),
        }
    }

    fn into_monitor(mut self) -> Result<DetachedJoinMonitor<T>, DetachedJoinMonitorError> {
        let join = self.join.take().ok_or(DetachedJoinMonitorError::Closed)?;
        spawn_detached_join_monitor(self.monitor_name, join)
    }
}

impl<T, F> Drop for DetachedJoinOwner<T, F>
where
    T: Copy + Send + Sync + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // Dropping the returned receiver detaches only the monitor result; the
        // OS thread continues to request and wait for actor shutdown.
        let _monitor = spawn_detached_join_monitor(self.monitor_name, join);
    }
}

enum FirstJoinAttempt<T> {
    Complete(Result<T, DetachedJoinMonitorError>),
    TimedOut(DetachedJoinMonitor<T>),
}

async fn first_join_attempt<T>(
    monitor: Result<DetachedJoinMonitor<T>, DetachedJoinMonitorError>,
    timeout: Duration,
) -> FirstJoinAttempt<T>
where
    T: Copy,
{
    let mut monitor = match monitor {
        Ok(monitor) => monitor,
        Err(error) => return FirstJoinAttempt::Complete(Err(error)),
    };
    match tokio::time::timeout(timeout, monitor.wait()).await {
        Ok(result) => FirstJoinAttempt::Complete(result),
        Err(_) => FirstJoinAttempt::TimedOut(monitor),
    }
}

async fn finish_join_attempt<T>(
    first: FirstJoinAttempt<T>,
    timeout: Duration,
) -> Result<T, ActorJoinWaitError>
where
    T: Copy,
{
    match first {
        FirstJoinAttempt::Complete(result) => result.map_err(ActorJoinWaitError::Monitor),
        FirstJoinAttempt::TimedOut(mut monitor) => tokio::time::timeout(timeout, monitor.wait())
            .await
            .map_err(|_| ActorJoinWaitError::TimedOut)?
            .map_err(ActorJoinWaitError::Monitor),
    }
}

enum ConfiguredViewer {
    Disabled,
    Enabled {
        origins: AllowedOrigins,
        tickets: Arc<InMemoryViewerTicketRegistry>,
        gateway: Arc<ViewerGateway>,
    },
}

impl ConfiguredViewer {
    const fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    fn into_router_parts(self, services: ApiServices) -> (AllowedOrigins, ApiServices) {
        match self {
            Self::Disabled => (AllowedOrigins::default(), services),
            Self::Enabled {
                origins,
                tickets,
                gateway,
            } => (
                origins,
                services
                    .with_viewer_ticket_service(tickets)
                    .with_viewer_gateway(gateway),
            ),
        }
    }
}

fn configured_viewer(config: &ViewerConfig) -> Result<ConfiguredViewer, DaemonError> {
    if !config.enabled() {
        return Ok(ConfiguredViewer::Disabled);
    }

    let origins = AllowedOrigins::exact(config.allowed_origins().iter().cloned())?;
    let registry_config = ViewerTicketRegistryConfig::new(
        config.ticket_capacity(),
        Duration::from_secs(config.ticket_ttl_seconds()),
    )?;
    let tickets = Arc::new(InMemoryViewerTicketRegistry::new(registry_config)?);
    let connector = Arc::new(LoopbackWebsockifyConnector::new(config.backend_address())?);
    let limits = ViewerGatewayLimits::new(
        Duration::from_millis(config.connect_timeout_ms()),
        Duration::from_millis(config.write_timeout_ms()),
        Duration::from_millis(config.idle_timeout_ms()),
        Duration::from_millis(config.session_timeout_ms()),
        config.maximum_frame_bytes(),
        config.maximum_sessions(),
    )?;
    let gateway = Arc::new(ViewerGateway::new(connector, config.no_vnc_root(), limits)?);
    Ok(ConfiguredViewer::Enabled {
        origins,
        tickets,
        gateway,
    })
}

fn configuration_environment(
    environment: impl IntoIterator<Item = (String, String)>,
) -> impl Iterator<Item = (String, String)> {
    environment
        .into_iter()
        .filter(|(key, _)| key != CONFIG_PATH_ENV)
}

#[derive(Debug, Error)]
enum DaemonError {
    #[error("could not read configuration file: {0}")]
    ReadConfig(std::io::Error),
    #[error(transparent)]
    Config(#[from] ConfigLoadError),
    #[error(transparent)]
    Principal(#[from] PrincipalError),
    #[error("validated authorization grant could not be mapped")]
    AuthorizationGrantInvariant,
    #[error(transparent)]
    Token(#[from] TokenLoadError),
    #[error(transparent)]
    TransportLimits(#[from] TransportLimitError),
    #[error(transparent)]
    RuntimeCapabilities(#[from] RuntimeCapabilityError),
    #[error(transparent)]
    AccessibilityRuntime(#[from] AccessibilityRuntimeError),
    #[error(transparent)]
    WindowCapabilityMonitor(#[from] WindowCapabilityMonitorError),
    #[error(transparent)]
    OperationBackendMonitor(#[from] OperationBackendMonitorError),
    #[error(transparent)]
    EventSinkBind(#[from] DeferredEventSinkBindError),
    #[error(transparent)]
    ClipboardEventRelay(#[from] ClipboardEventRelayError),
    #[error(transparent)]
    ViewerOriginPolicy(#[from] OriginPolicyError),
    #[error(transparent)]
    ViewerTickets(#[from] ViewerTicketRegistryError),
    #[error(transparent)]
    ViewerGateway(#[from] ViewerGatewayConfigurationError),
    #[error(transparent)]
    ActorJoinMonitor(#[from] DetachedJoinMonitorError),
    #[error(transparent)]
    ArtifactStore(#[from] StoreError),
    #[error(transparent)]
    ArtifactRetention(#[from] RetentionPolicyError),
    #[error(transparent)]
    ArtifactUploadTimeout(#[from] UploadTimeoutPolicyError),
    #[error(transparent)]
    ObservationComposition(#[from] ObservationCompositionError),
    #[error("window-control actor could not start: {0}")]
    WindowControlStartup(X11Error),
    #[error("window-control actor exited unexpectedly: {0:?}")]
    WindowControlRuntime(WindowControlActorExit),
    #[error("window-control actor did not stop before its shutdown deadline")]
    WindowControlShutdownTimeout,
    #[error("capture actor could not start: {0}")]
    CaptureStartup(X11Error),
    #[error("capture actor exited unexpectedly: {0:?}")]
    CaptureRuntime(CaptureActorExit),
    #[error("capture actor did not stop before its shutdown deadline")]
    CaptureShutdownTimeout,
    #[error("clipboard actor could not start: {0}")]
    ClipboardStartup(X11Error),
    #[error("clipboard actor exited unexpectedly: {0:?}")]
    ClipboardRuntime(ClipboardActorExit),
    #[error("clipboard actor did not stop before its shutdown deadline")]
    ClipboardShutdownTimeout,
    #[error("observation service exited unexpectedly: {0:?}")]
    ObservationRuntime(ObservationServiceExit),
    #[error("observation service did not stop before its shutdown deadline")]
    ObservationShutdownTimeout,
    #[error("startup worker failed: {0}")]
    StartupTask(String),
    #[error("invalid tracing filter: {0}")]
    LogFilter(tracing_subscriber::filter::ParseError),
    #[error("could not initialize tracing: {0}")]
    Tracing(String),
    #[error("could not bind HTTP listener: {0}")]
    Bind(std::io::Error),
    #[error(transparent)]
    ShutdownSignal(#[from] shutdown::ShutdownSignalError),
    #[error("HTTP server failed: {0}")]
    Serve(std::io::Error),
    #[error(transparent)]
    DesktopSupervisor(#[from] desktop_supervisor::DesktopSupervisorError),
    #[error(transparent)]
    CoordinatorSetup(#[from] control_plane::CoordinatorSetupError),
    #[error(transparent)]
    CoordinatorRuntime(#[from] control_plane::CoordinatorRuntimeError),
}

#[derive(Debug, Error)]
enum DetachedJoinMonitorError {
    #[error("could not start detached actor join monitor: {0}")]
    Spawn(std::io::Error),
    #[error("detached actor join monitor closed without an exit state")]
    Closed,
}

#[derive(Debug)]
enum ActorJoinWaitError {
    Monitor(DetachedJoinMonitorError),
    TimedOut,
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
