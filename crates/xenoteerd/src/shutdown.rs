//! Portable process shutdown-signal selection.

use core::fmt;

use thiserror::Error;

/// Operating-system signal that requested graceful shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    /// Terminal interrupt, normally Ctrl-C/SIGINT.
    Interrupt,
    /// Unix process termination request, normally Docker/s6 SIGTERM.
    #[cfg(unix)]
    Terminate,
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupt => formatter.write_str("interrupt"),
            #[cfg(unix)]
            Self::Terminate => formatter.write_str("terminate"),
        }
    }
}

/// Failure to install or receive process shutdown signals.
#[derive(Debug, Error)]
pub(crate) enum ShutdownSignalError {
    /// The platform could not install its Ctrl-C/SIGINT listener.
    #[cfg(not(unix))]
    #[error("could not receive interrupt signal: {0}")]
    Interrupt(std::io::Error),
    /// The Unix SIGINT listener could not be installed.
    #[cfg(unix)]
    #[error("could not install SIGINT listener: {0}")]
    InstallInterrupt(std::io::Error),
    /// The Unix SIGTERM listener could not be installed.
    #[cfg(unix)]
    #[error("could not install SIGTERM listener: {0}")]
    InstallTerminate(std::io::Error),
    /// The installed Unix SIGINT stream ended unexpectedly.
    #[cfg(unix)]
    #[error("SIGINT listener ended before receiving a signal")]
    InterruptStreamClosed,
    /// The installed Unix SIGTERM stream ended unexpectedly.
    #[cfg(unix)]
    #[error("SIGTERM listener ended before receiving a signal")]
    TerminateStreamClosed,
}

/// Installed operating-system shutdown listeners.
#[cfg(unix)]
pub(crate) struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    /// Installs SIGINT and SIGTERM listeners before the HTTP listener is bound.
    pub(crate) fn install() -> Result<Self, ShutdownSignalError> {
        use tokio::signal::unix::{SignalKind, signal};

        let interrupt =
            signal(SignalKind::interrupt()).map_err(ShutdownSignalError::InstallInterrupt)?;
        let terminate =
            signal(SignalKind::terminate()).map_err(ShutdownSignalError::InstallTerminate)?;
        Ok(Self {
            interrupt,
            terminate,
        })
    }

    /// Waits until an installed process signal requests graceful shutdown.
    pub(crate) async fn wait(mut self) -> Result<ShutdownSignal, ShutdownSignalError> {
        select_unix_shutdown(self.interrupt.recv(), self.terminate.recv()).await
    }
}

/// Portable Ctrl-C shutdown listener on non-Unix platforms.
#[cfg(not(unix))]
pub(crate) struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    /// Creates the platform fallback listener.
    pub(crate) fn install() -> Result<Self, ShutdownSignalError> {
        Ok(Self)
    }

    /// Waits for the portable Ctrl-C notification.
    pub(crate) async fn wait(self) -> Result<ShutdownSignal, ShutdownSignalError> {
        tokio::signal::ctrl_c()
            .await
            .map_err(ShutdownSignalError::Interrupt)?;
        Ok(ShutdownSignal::Interrupt)
    }
}

#[cfg(unix)]
async fn select_unix_shutdown<I, T>(
    interrupt: I,
    terminate: T,
) -> Result<ShutdownSignal, ShutdownSignalError>
where
    I: Future<Output = Option<()>>,
    T: Future<Output = Option<()>>,
{
    tokio::select! {
        result = interrupt => {
            result
                .map(|()| ShutdownSignal::Interrupt)
                .ok_or(ShutdownSignalError::InterruptStreamClosed)
        }
        result = terminate => {
            result
                .map(|()| ShutdownSignal::Terminate)
                .ok_or(ShutdownSignalError::TerminateStreamClosed)
        }
    }
}

#[cfg(unix)]
use std::future::Future;

#[cfg(all(test, unix))]
mod tests {
    use std::future::{pending, ready};

    use super::*;

    #[tokio::test]
    async fn selects_interrupt() -> Result<(), ShutdownSignalError> {
        let signal = select_unix_shutdown(ready(Some(())), pending::<Option<()>>()).await?;
        assert_eq!(signal, ShutdownSignal::Interrupt);
        Ok(())
    }

    #[tokio::test]
    async fn selects_terminate() -> Result<(), ShutdownSignalError> {
        let signal = select_unix_shutdown(pending::<Option<()>>(), ready(Some(()))).await?;
        assert_eq!(signal, ShutdownSignal::Terminate);
        Ok(())
    }

    #[tokio::test]
    async fn reports_closed_terminate_stream() {
        let error = select_unix_shutdown(pending::<Option<()>>(), ready(None)).await;
        assert!(matches!(
            error,
            Err(ShutdownSignalError::TerminateStreamClosed)
        ));
    }
}
