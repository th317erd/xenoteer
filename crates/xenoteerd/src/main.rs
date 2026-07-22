//! Xenoteer daemon composition entry point.

#![forbid(unsafe_code)]

mod control_plane;
mod desktop_supervisor;
#[allow(dead_code)] // Phase 3 routes wire this verified adapter in the next integration step.
mod process_manager;
mod shutdown;

use std::{fs, net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use xenoteer_core::{Config, ConfigLoadError, ConfigOverrides};
use xenoteer_protocol::{Capability, CapabilityId, CapabilityReport, CapabilityStatus};
use xenoteer_protocol::{DesktopGeneration, DesktopId};
use xenoteer_server::{
    AllowedOrigins, Authentication, DesktopReadiness, Grant, Principal, PrincipalError,
    ReadinessHandle, ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider,
    TokenLoadError, TransportLimitError, TransportLimits, api_router_with_control, serve,
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
    let capabilities = StaticCapabilityProvider::new(raw_control_capabilities()?);
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();

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
    let coordinator = control_plane::spawn(&config, desktop_id, desktop_generation, desktop_input)?;
    let coordinator_shutdown = coordinator.shutdown_handle();
    let desktop_cancellation = desktop_supervisor.cancellation();
    tracing::info!(
        listen = %local_address,
        readiness = "probing",
        reason_code = "desktop_capabilities_pending",
        "xenoteerd HTTP listener started"
    );

    let shutdown_readiness = readiness.clone();
    let application = api_router_with_control(
        readiness.clone(),
        desktop_id,
        authentication,
        capabilities,
        transport_limits,
        AllowedOrigins::default(),
        coordinator.control(),
    );
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
        desktop_cancellation.cancel();
    })
    .await;

    let coordinator_result = coordinator.shutdown().await;
    let supervisor_result = desktop_supervisor.shutdown().await;
    if let Err(error) = serve_result {
        return Err(DaemonError::Serve(error));
    }
    coordinator_result?;
    supervisor_result?;

    readiness.transition(ReadinessSnapshot::new(
        DesktopReadiness::Stopped,
        None,
        Some("shutdown_complete"),
    ));
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

fn raw_control_capabilities() -> Result<CapabilityReport, DaemonError> {
    let available = [
        "application.registered.launch",
        "input.keyboard.xtest",
        "input.pointer.smooth",
        "input.pointer.xtest",
        "input.reset.owned",
        "process.managed.status",
        "process.managed.terminate",
    ]
    .into_iter()
    .map(|id| CapabilityId::new(id).map(|id| Capability::new(id, CapabilityStatus::Available)))
    .collect::<Result<Vec<_>, _>>()?;
    Ok(CapabilityReport::checked(available)?)
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
    #[error(transparent)]
    CapabilityIdentifier(#[from] xenoteer_protocol::CapabilityIdError),
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
    Capabilities(#[from] xenoteer_protocol::CapabilityReportError),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_three_capabilities_are_explicit_and_available() -> Result<(), DaemonError> {
        let report = raw_control_capabilities()?;
        let identifiers = report
            .capabilities()
            .iter()
            .map(|capability| {
                assert_eq!(capability.status(), CapabilityStatus::Available);
                capability.id().as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identifiers,
            vec![
                "application.registered.launch",
                "input.keyboard.xtest",
                "input.pointer.smooth",
                "input.pointer.xtest",
                "input.reset.owned",
                "process.managed.status",
                "process.managed.terminate",
            ]
        );
        Ok(())
    }

    #[test]
    fn launcher_config_key_is_not_forwarded_to_typed_configuration() -> Result<(), ConfigLoadError>
    {
        let config = Config::load(
            None,
            configuration_environment([
                (
                    CONFIG_PATH_ENV.to_owned(),
                    "LAUNCHER_CONFIG_VALUE_CANARY".to_owned(),
                ),
                ("XENOTEER__LOGGING__FILTER".to_owned(), "warn".to_owned()),
            ]),
            &ConfigOverrides::default(),
        )?;
        assert_eq!(config.logging().filter(), "warn");
        Ok(())
    }

    #[test]
    fn configured_principal_honors_least_privilege_grants() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = Config::load(
            Some("[auth]\ngrants = ['desktop:status']"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        )?;
        let principal = configured_principal(&config)?;
        assert!(principal.has_grant(Grant::DesktopStatus));
        assert!(!principal.has_grant(Grant::DesktopObserve));
        assert!(!principal.has_grant(Grant::InputControl));
        Ok(())
    }

    #[test]
    fn unknown_xenoteer_environment_is_redacted_through_daemon_error()
    -> Result<(), Box<dyn std::error::Error>> {
        const KEY_CANARY: &str = "XENOTEER_BAD_KEY_SECRET_CANARY";
        const VALUE_CANARY: &str = "UNKNOWN_ENV_VALUE_SECRET_CANARY";
        let result = Config::load(
            None,
            configuration_environment([(KEY_CANARY.to_owned(), VALUE_CANARY.to_owned())]),
            &ConfigOverrides::default(),
        );
        let error = match result {
            Err(error) => DaemonError::Config(error),
            Ok(_) => {
                return Err(std::io::Error::other(
                    "unknown Xenoteer environment key unexpectedly loaded",
                )
                .into());
            }
        };
        assert_error_chain_redacted(&error, KEY_CANARY);
        assert_error_chain_redacted(&error, VALUE_CANARY);
        Ok(())
    }

    fn assert_error_chain_redacted(error: &DaemonError, canary: &str) {
        assert!(!format!("{error}").contains(canary));
        assert!(!format!("{error:?}").contains(canary));
        let mut source = std::error::Error::source(error);
        while let Some(current) = source {
            assert!(!format!("{current}").contains(canary));
            assert!(!format!("{current:?}").contains(canary));
            source = current.source();
        }
    }
}
