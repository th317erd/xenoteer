//! Connected client and immutable generation-bound desktop object.

use std::{sync::Arc, time::Duration};

use xenoteer_protocol::{
    Command, CommandEnvelope, CommandId, ControlLeaseId, DesktopGeneration, DesktopId,
    ProtocolVersion, RequestId, StatusResponse, Timestamp,
};

use crate::{Client, CommandHandle, CommandSubmission, SdkError};

/// Validated authenticated connection metadata and high-level object root.
#[derive(Clone, Debug)]
pub struct XenoteerClient {
    transport: Client,
    status: Arc<StatusResponse>,
    negotiated_protocol: ProtocolVersion,
    received_at: tokio::time::Instant,
    status_round_trip: Duration,
}

impl XenoteerClient {
    /// Connects, validates status, and negotiates the frozen v1 protocol.
    pub async fn connect(
        base_uri: impl AsRef<str>,
        bearer_token: impl AsRef<[u8]>,
    ) -> Result<Self, SdkError> {
        let transport = Client::new(base_uri, bearer_token)?;
        Self::from_transport(transport).await
    }

    /// Connects through an already configured retry-neutral transport.
    pub async fn from_transport(transport: Client) -> Result<Self, SdkError> {
        let started = tokio::time::Instant::now();
        let status = transport.status().await?;
        let received_at = tokio::time::Instant::now();
        let status_round_trip = received_at.saturating_duration_since(started);
        let server_range = xenoteer_protocol::VersionRange::new(
            status.protocol_min.major(),
            status.protocol_min.minor(),
            status.protocol_max.minor(),
        )
        .map_err(|_| SdkError::InvalidResponse)?;
        let negotiated_protocol = transport
            .protocol_range()
            .negotiate(server_range)
            .map_err(|_| SdkError::UnsupportedProtocol)?;
        Ok(Self {
            transport,
            status: Arc::new(status),
            negotiated_protocol,
            received_at,
            status_round_trip,
        })
    }

    /// Returns the validated status snapshot used for negotiation.
    #[must_use]
    pub fn status(&self) -> &StatusResponse {
        &self.status
    }

    /// Returns the selected protocol version.
    #[must_use]
    pub const fn negotiated_protocol(&self) -> ProtocolVersion {
        self.negotiated_protocol
    }

    /// Returns a generation-bound desktop handle when a session exists.
    pub fn desktop(&self) -> Result<Desktop, SdkError> {
        let generation = self
            .status
            .desktop
            .generation
            .ok_or(SdkError::DesktopUnavailable)?;
        Ok(Desktop {
            transport: self.transport.clone(),
            id: self.status.desktop.id,
            generation,
            protocol: self.negotiated_protocol,
        })
    }

    /// Converts a relative duration into a conservative absolute server deadline.
    ///
    /// The full measured status round trip is included as clock uncertainty;
    /// callers should refresh status before deriving long-lived deadlines.
    pub fn deadline_after(&self, duration: Duration) -> Result<Timestamp, SdkError> {
        if duration.is_zero() || duration > Duration::from_secs(3_600) {
            return Err(SdkError::InvalidRequest);
        }
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.received_at);
        let offset = elapsed
            .checked_add(self.status_round_trip)
            .and_then(|value| value.checked_add(duration))
            .ok_or(SdkError::InvalidRequest)?;
        let offset_nanos =
            i128::try_from(offset.as_nanos()).map_err(|_| SdkError::InvalidRequest)?;
        let deadline = self
            .status
            .server_time
            .unix_timestamp_nanos()
            .map_err(|_| SdkError::InvalidResponse)?
            .checked_add(offset_nanos)
            .ok_or(SdkError::InvalidRequest)?;
        Timestamp::from_unix_timestamp_nanos(deadline).map_err(|_| SdkError::InvalidRequest)
    }

    /// Closes the shared transport and its owned event supervisors.
    pub async fn close(&self) {
        self.transport.close().await;
    }

    /// Returns whether this client and all derived objects have been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.transport.is_closed()
    }
}

/// Cheap immutable handle for one exact desktop lifetime.
#[derive(Clone, Debug)]
pub struct Desktop {
    pub(crate) transport: Client,
    id: DesktopId,
    generation: DesktopGeneration,
    protocol: ProtocolVersion,
}

impl Desktop {
    pub(crate) fn for_test(
        transport: Client,
        id: DesktopId,
        generation: DesktopGeneration,
    ) -> Self {
        Self {
            transport,
            id,
            generation,
            protocol: ProtocolVersion::V1_0,
        }
    }

    /// Returns the stable desktop identity.
    #[must_use]
    pub const fn id(&self) -> DesktopId {
        self.id
    }

    /// Returns the exact desktop lifetime.
    #[must_use]
    pub const fn generation(&self) -> DesktopGeneration {
        self.generation
    }

    /// Prepares a command with a fresh caller-side ID and no network I/O.
    pub fn submit(&self, command: Command) -> Result<CommandSubmission, SdkError> {
        self.prepare_with(CommandId::new(), None, None, command)
    }

    /// Prepares a controlled command with a fresh caller-side ID and no I/O.
    pub fn submit_controlled(
        &self,
        lease_id: ControlLeaseId,
        command: Command,
    ) -> Result<CommandSubmission, SdkError> {
        self.prepare_with(CommandId::new(), Some(lease_id), None, command)
    }

    /// Submits an exact caller-selected ID for safe deduplication/recovery.
    pub async fn submit_with(
        &self,
        command_id: CommandId,
        lease_id: Option<ControlLeaseId>,
        deadline: Option<Timestamp>,
        command: Command,
    ) -> Result<CommandHandle, SdkError> {
        self.prepare_with(command_id, lease_id, deadline, command)?
            .send()
            .await
    }

    /// Prepares an exact caller-selected ID without performing network I/O.
    pub fn prepare_with(
        &self,
        command_id: CommandId,
        lease_id: Option<ControlLeaseId>,
        deadline: Option<Timestamp>,
        command: Command,
    ) -> Result<CommandSubmission, SdkError> {
        let mut envelope = if let Some(lease_id) = lease_id {
            CommandEnvelope::new_with_lease(
                self.protocol,
                RequestId::new(),
                command_id,
                self.id,
                self.generation,
                lease_id,
                command,
            )
        } else {
            CommandEnvelope::new(
                self.protocol,
                RequestId::new(),
                command_id,
                self.id,
                self.generation,
                command,
            )
        }
        .map_err(|_| SdkError::InvalidRequest)?;
        if let Some(deadline) = deadline {
            envelope = envelope.with_deadline(deadline);
        }
        envelope.validate().map_err(|_| SdkError::InvalidRequest)?;
        Ok(CommandSubmission::new(
            self.transport.clone(),
            self.id,
            self.generation,
            envelope,
        ))
    }

    /// Reattaches to an existing command ID without resubmitting it.
    pub async fn command(&self, command_id: CommandId) -> Result<CommandHandle, SdkError> {
        let result = self.transport.get_command(self.id, command_id).await?;
        Ok(CommandHandle::new(
            self.transport.clone(),
            self.id,
            self.generation,
            result,
        ))
    }

    pub(crate) const fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }
}
