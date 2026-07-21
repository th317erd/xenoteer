//! Xenoteer daemon composition entry point.

#![forbid(unsafe_code)]

mod desktop_supervisor;
mod shutdown;

use std::{fs, net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use xenoteer_core::{Config, ConfigLoadError, ConfigOverrides};
use xenoteer_server::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot, router, serve};

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
    let (desktop_supervisor, mut desktop_fatal) =
        desktop_supervisor::spawn(readiness.clone(), desktop_spec);
    let desktop_cancellation = desktop_supervisor.cancellation();
    tracing::info!(
        listen = %local_address,
        readiness = "probing",
        reason_code = "desktop_capabilities_pending",
        "xenoteerd HTTP listener started"
    );

    let shutdown_readiness = readiness.clone();
    let serve_result = serve(listener, router(readiness.clone()), async move {
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
        desktop_cancellation.cancel();
        shutdown_readiness.transition(ReadinessSnapshot::new(
            DesktopReadiness::Draining,
            None,
            Some("shutdown_in_progress"),
        ));
    })
    .await;

    let supervisor_result = desktop_supervisor.shutdown().await;
    if let Err(error) = serve_result {
        return Err(DaemonError::Serve(error));
    }
    supervisor_result?;

    readiness.transition(ReadinessSnapshot::new(
        DesktopReadiness::Stopped,
        None,
        Some("shutdown_complete"),
    ));
    tracing::info!("xenoteerd stopped");
    Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
