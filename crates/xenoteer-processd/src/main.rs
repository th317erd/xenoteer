//! Root-supervised application broker entry point.

#![forbid(unsafe_code)]

use std::{os::unix::fs::MetadataExt, process::ExitCode, time::Duration};

use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use xenoteer_processd::{BrokerClient, DEFAULT_BROKER_SOCKET, bind_image_broker};

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = run().await {
        eprintln!("xenoteer-processd failed: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    match arguments.next() {
        Some(argument) if argument == "--probe" && arguments.next().is_none() => {
            timeout(
                Duration::from_secs(2),
                BrokerClient::new(DEFAULT_BROKER_SOCKET).probe(),
            )
            .await??;
            return Ok(());
        }
        Some(_) => return Err("the broker accepts only --probe".into()),
        None => {}
    }

    if std::fs::metadata("/proc/self")?.uid() != 0 {
        return Err("the process broker must run as root".into());
    }
    // The broker needs effective root only for fixed child credential changes.
    // GUI children inherit no root or service-group supplementary membership.
    nix::unistd::setgroups(&[])?;
    nix::sys::prctl::set_no_new_privs()?;
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init()?;

    let cancellation = CancellationToken::new();
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        let terminate = async {
            let mut signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let _received = signal.recv().await;
            Ok::<(), std::io::Error>(())
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(%error, "failed to install broker interrupt signal");
                }
            }
            result = terminate => {
                if let Err(error) = result {
                    tracing::error!(%error, "failed to install broker termination signal");
                }
            }
        }
        signal_cancellation.cancel();
    });

    let server = bind_image_broker().await?;
    tracing::info!(
        socket = DEFAULT_BROKER_SOCKET,
        "application process broker ready"
    );
    let serve_result = server.serve(cancellation.clone()).await;
    cancellation.cancel();
    if !signal_task.is_finished() {
        signal_task.abort();
    }
    match signal_task.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error.into()),
    }
    serve_result?;
    tracing::info!("application process broker stopped");
    Ok(())
}
