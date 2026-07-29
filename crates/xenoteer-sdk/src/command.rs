//! Cancel-safe handles for accepted, deduplicated commands.

use std::{fmt, time::Duration};

use xenoteer_protocol::{CommandEnvelope, CommandId, CommandResult, DesktopGeneration, DesktopId};

use crate::{Client, SdkError};

/// A fully validated mutation whose deduplication ID is available before I/O.
///
/// Callers should persist or print [`Self::id`] before awaiting [`Self::send`].
/// If the exchange fails ambiguously, reattach with [`crate::Desktop::command`]
/// instead of silently creating a second command.
pub struct CommandSubmission {
    client: Client,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    envelope: CommandEnvelope,
}

impl CommandSubmission {
    pub(crate) fn new(
        client: Client,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        envelope: CommandEnvelope,
    ) -> Self {
        Self {
            client,
            desktop_id,
            desktop_generation,
            envelope,
        }
    }

    /// Returns the immutable server-side deduplication identity before any I/O.
    #[must_use]
    pub const fn id(&self) -> CommandId {
        self.envelope.command_id
    }

    /// Returns the desktop lifetime captured before any submission I/O.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Explicitly fences this retained submission against a refreshed desktop.
    ///
    /// Call this after reconnect/status refresh before lookup or an explicit
    /// resend. A mismatch never mutates or sends the retained envelope.
    pub fn ensure_generation(&self, current: DesktopGeneration) -> Result<(), SdkError> {
        if current != self.desktop_generation {
            return Err(SdkError::GenerationChanged);
        }
        Ok(())
    }

    /// Makes exactly one submission attempt and never replays it automatically.
    ///
    /// The validated submission remains available after transport ambiguity so
    /// the caller can first query [`Self::id`] and explicitly resend this exact
    /// envelope only after establishing that it was not accepted.
    pub async fn send(&self) -> Result<CommandHandle, SdkError> {
        let result = self.client.submit_command(&self.envelope).await?;
        Ok(CommandHandle::new(
            self.client.clone(),
            self.desktop_id,
            self.desktop_generation,
            result,
        ))
    }
}

impl fmt::Debug for CommandSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSubmission")
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .field("command_id", &self.id())
            .finish_non_exhaustive()
    }
}

/// A generation-bound command whose server identity survives local task cancellation.
///
/// Dropping this handle never sends a server cancellation. Call [`Self::cancel`]
/// explicitly when cancellation of the remote command is intended.
#[derive(Clone, Debug)]
pub struct CommandHandle {
    client: Client,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    command_id: CommandId,
    latest: CommandResult,
}

impl CommandHandle {
    pub(crate) fn new(
        client: Client,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        latest: CommandResult,
    ) -> Self {
        Self {
            command_id: latest.command_id(),
            client,
            desktop_id,
            desktop_generation,
            latest,
        }
    }

    /// Returns the immutable server-side deduplication identity.
    #[must_use]
    pub const fn id(&self) -> CommandId {
        self.command_id
    }

    /// Returns the desktop lifetime to which this handle is fenced.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns the most recently observed validated result.
    #[must_use]
    pub const fn latest(&self) -> &CommandResult {
        &self.latest
    }

    /// Refreshes the command without submitting or replaying it.
    pub async fn refresh(&mut self) -> Result<&CommandResult, SdkError> {
        self.latest = self
            .client
            .get_command(self.desktop_id, self.command_id)
            .await?;
        Ok(&self.latest)
    }

    /// Performs one bounded server-side wait without creating new work.
    pub async fn wait_once(&mut self, timeout: Duration) -> Result<&CommandResult, SdkError> {
        let timeout_ms = u32::try_from(timeout.as_millis())
            .ok()
            .filter(|value| (1..=crate::MAX_WAIT_TIMEOUT_MS).contains(value))
            .ok_or(SdkError::InvalidRequest)?;
        self.latest = self
            .client
            .wait_command(self.desktop_id, self.command_id, timeout_ms)
            .await?;
        Ok(&self.latest)
    }

    /// Waits until the command is terminal or the local overall bound elapses.
    ///
    /// A local timeout does not imply server cancellation and is safe to follow
    /// with [`Self::refresh`] or another wait on the same handle.
    pub async fn wait_terminal(
        &mut self,
        overall_timeout: Duration,
    ) -> Result<&CommandResult, SdkError> {
        if overall_timeout.is_zero() || overall_timeout > Duration::from_secs(3_600) {
            return Err(SdkError::InvalidRequest);
        }
        let deadline = tokio::time::Instant::now() + overall_timeout;
        while !self.latest.lifecycle().is_terminal() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(SdkError::CommandWaitTimeout);
            }
            let slice = remaining.min(Duration::from_millis(u64::from(crate::MAX_WAIT_TIMEOUT_MS)));
            match tokio::time::timeout(remaining, self.wait_once(slice)).await {
                Ok(result) => {
                    result?;
                }
                Err(_) => return Err(SdkError::CommandWaitTimeout),
            }
        }
        Ok(&self.latest)
    }

    /// Requests cooperative server-side cancellation for this exact command ID.
    pub async fn cancel(&mut self) -> Result<&CommandResult, SdkError> {
        self.latest = self
            .client
            .cancel_command(self.desktop_id, self.command_id)
            .await?;
        Ok(&self.latest)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Instant};

    use tokio::{
        io::AsyncReadExt,
        net::TcpListener,
        time::{sleep, timeout},
    };
    use xenoteer_protocol::{
        ApplicationArgument, ApplicationId, ApplicationLaunchCommand, Command, CommandEnvelope,
        CommandResult, ProtocolVersion, RequestId, Timestamp,
    };

    use super::*;

    type TestError = Box<dyn Error + Send + Sync>;

    fn test_token() -> &'static str {
        "command-test-token-0123456789abcdef"
    }

    fn submission(client: Client, command: Command) -> Result<CommandSubmission, TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let envelope = CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            command,
        )?;
        Ok(CommandSubmission::new(
            client, desktop_id, generation, envelope,
        ))
    }

    #[test]
    fn submission_debug_never_exposes_command_payload() -> Result<(), TestError> {
        let canary = "command-debug-canary-never-print";
        let command = Command::ApplicationLaunch(ApplicationLaunchCommand {
            application: ApplicationId::new("fixture")?,
            arguments: vec![ApplicationArgument::new(canary)?],
        });
        let value = submission(Client::new("http://127.0.0.1:8080", test_token())?, command)?;
        let debug = format!("{value:?}");
        assert!(debug.contains(&value.id().to_string()));
        assert!(!debug.contains(canary));
        Ok(())
    }

    #[test]
    fn submission_explicitly_fences_a_reconnected_generation() -> Result<(), TestError> {
        let value = submission(
            Client::new("http://127.0.0.1:8080", test_token())?,
            Command::DesktopProbe(xenoteer_protocol::DesktopProbeCommand {}),
        )?;
        assert!(value.ensure_generation(value.desktop_generation()).is_ok());
        assert!(matches!(
            value.ensure_generation(DesktopGeneration::new()),
            Err(SdkError::GenerationChanged)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_send_preserves_exact_submission_for_caller_decision() -> Result<(), TestError>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await?;
                let mut request = [0_u8; 4096];
                let _read = timeout(Duration::from_secs(1), stream.read(&mut request)).await??;
                drop(stream);
            }
            Ok::<(), std::io::Error>(())
        });
        let submission = submission(
            Client::new(base, test_token())?,
            Command::DesktopProbe(xenoteer_protocol::DesktopProbeCommand {}),
        )?;
        let command_id = submission.id();

        assert!(matches!(submission.send().await, Err(SdkError::Transport)));
        assert_eq!(submission.id(), command_id);
        assert!(matches!(submission.send().await, Err(SdkError::Transport)));
        assert_eq!(submission.id(), command_id);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn overall_wait_deadline_bounds_a_stalled_http_exchange() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await?;
            sleep(Duration::from_secs(5)).await;
            Ok::<(), std::io::Error>(())
        });
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let latest = CommandResult::accepted(command_id, Timestamp::parse("2026-07-23T00:00:00Z")?);
        let mut handle = CommandHandle::new(
            Client::new(base, test_token())?,
            desktop_id,
            generation,
            latest,
        );
        let started = Instant::now();
        assert!(matches!(
            handle.wait_terminal(Duration::from_millis(30)).await,
            Err(SdkError::CommandWaitTimeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        server.abort();
        let _aborted = server.await;
        Ok(())
    }
}
