//! Explicit async controller-lease lifecycle.

use std::fmt;

use xenoteer_protocol::{
    Command, CommandId, ControlLeaseId, LeaseAcquireRequest, LeaseAvailability,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, RequestId, Timestamp,
};

use crate::{CommandHandle, CommandSubmission, Desktop, SdkError};

/// An owned generation-bound controller lease.
///
/// Dropping this value cannot await and therefore does not claim guaranteed
/// release. Use [`Self::release`] for deterministic cleanup.
pub struct ControlLease {
    desktop: Desktop,
    lease_id: ControlLeaseId,
    ttl_ms: Option<u32>,
    expires_at: Timestamp,
    active: bool,
}

impl fmt::Debug for ControlLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlLease")
            .field("desktop", &self.desktop)
            .field("lease_id", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("active", &self.active)
            .finish()
    }
}

impl ControlLease {
    pub(crate) fn from_acquire(
        desktop: Desktop,
        ttl_ms: Option<u32>,
        state: LeaseStateView,
    ) -> Result<Self, SdkError> {
        state.validate().map_err(|_| SdkError::InvalidResponse)?;
        if state.desktop_id != desktop.id()
            || state.desktop_generation != desktop.generation()
            || state.state != LeaseAvailability::HeldByCaller
        {
            return Err(SdkError::InvalidResponse);
        }
        Ok(Self {
            desktop,
            lease_id: state.lease_id.ok_or(SdkError::InvalidResponse)?,
            ttl_ms,
            expires_at: state.expires_at.ok_or(SdkError::InvalidResponse)?,
            active: true,
        })
    }

    /// Returns the opaque capability used by controlled command envelopes.
    #[must_use]
    pub const fn id(&self) -> ControlLeaseId {
        self.lease_id
    }

    /// Returns the latest server-observed expiry.
    #[must_use]
    pub const fn expires_at(&self) -> &Timestamp {
        &self.expires_at
    }

    /// Returns whether this local handle has not been explicitly released.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Renews the same lease capability; renewal failures remain visible.
    pub async fn renew(&mut self) -> Result<&Timestamp, SdkError> {
        if !self.active {
            return Err(SdkError::LeaseReleased);
        }
        let request = LeaseRenewRequest {
            protocol_version: self.desktop.protocol(),
            request_id: RequestId::new(),
            desktop_id: self.desktop.id(),
            desktop_generation: self.desktop.generation(),
            lease_id: self.lease_id,
            ttl_ms: self.ttl_ms,
        };
        let state = self.desktop.transport.renew_lease(&request).await?;
        if state.state != LeaseAvailability::HeldByCaller || state.lease_id != Some(self.lease_id) {
            return Err(SdkError::InvalidResponse);
        }
        self.expires_at = state.expires_at.ok_or(SdkError::InvalidResponse)?;
        Ok(&self.expires_at)
    }

    /// Explicitly releases the lease and waits for server-side owned-input reset.
    pub async fn release(&mut self) -> Result<LeaseStateView, SdkError> {
        if !self.active {
            return Err(SdkError::LeaseReleased);
        }
        let request = LeaseReleaseRequest {
            protocol_version: self.desktop.protocol(),
            request_id: RequestId::new(),
            desktop_id: self.desktop.id(),
            desktop_generation: self.desktop.generation(),
            lease_id: self.lease_id,
        };
        // Keep the capability active until a valid server response proves the
        // release. A timeout or disconnect is ambiguous and callers must be
        // able to query/retry with this exact lease ID.
        let state = self.desktop.transport.release_lease(&request).await?;
        if state.state == LeaseAvailability::HeldByCaller {
            return Err(SdkError::InvalidResponse);
        }
        self.active = false;
        Ok(state)
    }

    /// Prepares one controlled command so its ID is visible before network I/O.
    pub fn submit(&self, command: Command) -> Result<CommandSubmission, SdkError> {
        if !self.active {
            return Err(SdkError::LeaseReleased);
        }
        self.desktop.submit_controlled(self.lease_id, command)
    }

    /// Submits one controlled command with an explicit deduplication ID/deadline.
    pub async fn submit_with(
        &self,
        command_id: CommandId,
        deadline: Option<Timestamp>,
        command: Command,
    ) -> Result<CommandHandle, SdkError> {
        if !self.active {
            return Err(SdkError::LeaseReleased);
        }
        self.desktop
            .submit_with(command_id, Some(self.lease_id), deadline, command)
            .await
    }
}

impl Desktop {
    /// Acquires exclusive physical-input control without automatic renewal.
    pub async fn acquire_control(&self, ttl_ms: Option<u32>) -> Result<ControlLease, SdkError> {
        let request = LeaseAcquireRequest {
            protocol_version: self.protocol(),
            request_id: RequestId::new(),
            desktop_id: self.id(),
            desktop_generation: self.generation(),
            ttl_ms,
        };
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        let state = self.transport.acquire_lease(&request).await?;
        ControlLease::from_acquire(self.clone(), ttl_ms, state)
    }

    /// Reads caller-redacted lease state for this exact desktop lifetime.
    pub async fn control_state(&self) -> Result<LeaseStateView, SdkError> {
        self.transport
            .lease_state(self.id(), self.generation())
            .await
    }

    /// Renews an existing caller-owned lease capability.
    pub async fn renew_control(
        &self,
        lease_id: ControlLeaseId,
        ttl_ms: Option<u32>,
    ) -> Result<LeaseStateView, SdkError> {
        let request = LeaseRenewRequest {
            protocol_version: self.protocol(),
            request_id: RequestId::new(),
            desktop_id: self.id(),
            desktop_generation: self.generation(),
            lease_id,
            ttl_ms,
        };
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        self.transport.renew_lease(&request).await
    }

    /// Explicitly releases an existing caller-owned lease capability.
    pub async fn release_control(
        &self,
        lease_id: ControlLeaseId,
    ) -> Result<LeaseStateView, SdkError> {
        let request = LeaseReleaseRequest {
            protocol_version: self.protocol(),
            request_id: RequestId::new(),
            desktop_id: self.id(),
            desktop_generation: self.generation(),
            lease_id,
        };
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        self.transport.release_lease(&request).await
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Duration};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };
    use xenoteer_protocol::{DesktopGeneration, DesktopId};

    use super::*;
    use crate::Client;

    type TestError = Box<dyn Error + Send + Sync>;

    #[tokio::test]
    async fn ambiguous_release_retains_the_exact_lease_capability() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 4096];
            let _read = timeout(Duration::from_secs(1), stream.read(&mut request)).await??;
            drop(stream);
            Ok::<(), std::io::Error>(())
        });
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let state = LeaseStateView {
            desktop_id,
            desktop_generation: generation,
            state: LeaseAvailability::HeldByCaller,
            lease_id: Some(lease_id),
            expires_at: Some(Timestamp::parse("2026-07-23T01:00:00Z")?),
        };
        let mut lease = ControlLease::from_acquire(desktop, Some(30_000), state)?;
        assert!(matches!(lease.release().await, Err(SdkError::Transport)));
        assert!(lease.is_active());
        assert_eq!(lease.id(), lease_id);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn release_response_that_still_holds_the_lease_is_not_confirmation()
    -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let held = LeaseStateView {
            desktop_id,
            desktop_generation: generation,
            state: LeaseAvailability::HeldByCaller,
            lease_id: Some(lease_id),
            expires_at: Some(Timestamp::parse("2026-07-23T01:00:00Z")?),
        };
        let response_body = serde_json::to_vec(&held)?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 4096];
            let _read = timeout(Duration::from_secs(1), stream.read(&mut request)).await??;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            stream.write_all(&response_body).await?;
            Ok::<(), std::io::Error>(())
        });
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let mut lease = ControlLease::from_acquire(desktop, Some(30_000), held)?;
        assert!(matches!(
            lease.release().await,
            Err(SdkError::InvalidResponse)
        ));
        assert!(lease.is_active());
        assert_eq!(lease.id(), lease_id);
        server.await??;
        Ok(())
    }
}
