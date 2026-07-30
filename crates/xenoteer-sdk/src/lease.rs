//! Explicit and scoped async controller-lease lifecycle.

use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use xenoteer_protocol::{
    Command, CommandId, ControlLeaseId, LeaseAcquireRequest, LeaseAvailability,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, RequestId, Timestamp,
};

use crate::{CommandHandle, CommandSubmission, Desktop, SdkError};

/// Boxed callback future accepted by [`Desktop::with_control`].
///
/// Boxing makes the lease-borrow lifetime explicit without a detached task or
/// a lifetime-erasing callback boundary.
pub type ControlScopeFuture<'scope, T, E> =
    Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'scope>>;

/// Maximum time a callback may cooperatively stop after lease renewal fails.
///
/// The actual grace is the smaller of this bound and the normal renewal
/// interval. After it expires, the callback is dropped before release begins.
pub const MAX_CONTROL_SCOPE_RENEWAL_FAILURE_GRACE: Duration = Duration::from_millis(250);

/// Lease-bound controlled operations exposed only during [`Desktop::with_control`].
///
/// Every operation checks the scope's renewal state before preparing new work.
/// Already-submitted server commands retain their ordinary command-ID recovery
/// contract if a later lease renewal fails.
pub struct ScopedControl<'scope> {
    lease: &'scope ControlLease,
    renewal_failed: Arc<AtomicBool>,
    in_flight: Arc<Mutex<BTreeMap<CommandId, usize>>>,
}

impl fmt::Debug for ScopedControl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedControl")
            .field("lease", &"<redacted>")
            .field("healthy", &!self.renewal_failed.load(Ordering::Acquire))
            .finish()
    }
}

impl ScopedControl<'_> {
    /// Fails immediately after the scope has observed a renewal failure.
    pub fn ensure_healthy(&self) -> Result<(), SdkError> {
        if self.renewal_failed.load(Ordering::Acquire) {
            return Err(SdkError::ControlLeaseRenewalFailed);
        }
        Ok(())
    }

    /// Prepares one lifetime-bound controlled command while renewal is healthy.
    ///
    /// The returned wrapper borrows this scope, so a lease-bearing submission
    /// cannot be returned from the scoped callback and sent after release.
    pub fn submit(&self, command: Command) -> Result<ScopedCommandSubmission<'_>, SdkError> {
        self.ensure_healthy()?;
        Ok(ScopedCommandSubmission {
            submission: self.lease.submit(command)?,
            renewal_failed: self.renewal_failed.as_ref(),
            in_flight: self.in_flight.as_ref(),
        })
    }
}

/// A controlled submission that cannot outlive its [`ScopedControl`].
pub struct ScopedCommandSubmission<'scope> {
    submission: CommandSubmission,
    renewal_failed: &'scope AtomicBool,
    in_flight: &'scope Mutex<BTreeMap<CommandId, usize>>,
}

impl fmt::Debug for ScopedCommandSubmission<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedCommandSubmission")
            .field("command_id", &self.id())
            .field("desktop_generation", &self.desktop_generation())
            .finish_non_exhaustive()
    }
}

impl ScopedCommandSubmission<'_> {
    /// Returns the immutable command ID before network I/O.
    ///
    /// Persist this value before awaiting [`Self::send`]. If renewal fails
    /// while the send is pending, scoped cleanup also retains this exact ID as
    /// an ambiguous in-flight submission.
    #[must_use]
    pub const fn id(&self) -> CommandId {
        self.submission.id()
    }

    /// Returns the desktop lifetime captured by the controlled envelope.
    #[must_use]
    pub const fn desktop_generation(&self) -> xenoteer_protocol::DesktopGeneration {
        self.submission.desktop_generation()
    }

    /// Sends exactly once only while the enclosing scope remains healthy.
    ///
    /// A send still pending when the renewal-failure grace expires is dropped
    /// locally, not cancelled on the server. Its immutable ID is exposed by
    /// [`ControlScopeCleanupError::aborted_in_flight_command_ids`] for explicit
    /// status recovery.
    pub async fn send(&self) -> Result<CommandHandle, SdkError> {
        if self.renewal_failed.load(Ordering::Acquire) {
            return Err(SdkError::ControlLeaseRenewalFailed);
        }
        let command_id = self.id();
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight.entry(command_id).or_insert(0) += 1;
        }
        let result = self.submission.send().await;
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let remove = if let Some(count) = in_flight.get_mut(&command_id) {
                *count -= 1;
                *count == 0
            } else {
                false
            };
            if remove {
                in_flight.remove(&command_id);
            }
        }
        result
    }
}

/// Why a scoped callback was forcibly stopped before release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlScopeCallbackAbort {
    /// Renewal failed and the callback did not finish within its bounded grace.
    RenewalFailureGraceExpired {
        /// The actual grace allowed before the callback was dropped.
        grace: Duration,
    },
}

/// Cleanup failures observed after a scoped callback stopped running.
pub struct ControlScopeCleanupError {
    lease_id: ControlLeaseId,
    renewal: Option<SdkError>,
    release: Option<SdkError>,
    callback_abort: Option<ControlScopeCallbackAbort>,
    aborted_in_flight_command_ids: Vec<CommandId>,
}

impl ControlScopeCleanupError {
    /// Returns the exact lease capability for explicit cleanup reconciliation.
    ///
    /// If [`Self::release_error`] reports an ambiguous timeout or disconnect,
    /// query [`Desktop::control_state`] and, when required, retry
    /// [`Desktop::release_control`] with this same ID. Treat the value as a
    /// capability: persist it only as needed and do not log it. Debug and
    /// display formatting deliberately redact it.
    #[must_use]
    pub const fn lease_id(&self) -> ControlLeaseId {
        self.lease_id
    }

    /// Returns the renewal failure that fenced subsequent controlled work.
    #[must_use]
    pub const fn renewal_error(&self) -> Option<&SdkError> {
        self.renewal.as_ref()
    }

    /// Returns the deterministic awaited release failure.
    #[must_use]
    pub const fn release_error(&self) -> Option<&SdkError> {
        self.release.as_ref()
    }

    /// Returns evidence that cleanup forcibly dropped a noncooperative callback.
    #[must_use]
    pub const fn callback_abort(&self) -> Option<ControlScopeCallbackAbort> {
        self.callback_abort
    }

    /// Returns exact IDs whose submission exchanges were pending when aborted.
    ///
    /// Their server-side acceptance is ambiguous. Query each ID with
    /// [`Desktop::command`](crate::Desktop::command) before deciding the next
    /// action; scoped cleanup never replays it automatically.
    #[must_use]
    pub fn aborted_in_flight_command_ids(&self) -> &[CommandId] {
        &self.aborted_in_flight_command_ids
    }

    const fn is_empty(&self) -> bool {
        self.renewal.is_none()
            && self.release.is_none()
            && self.callback_abort.is_none()
            && self.aborted_in_flight_command_ids.is_empty()
    }
}

impl fmt::Debug for ControlScopeCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlScopeCleanupError")
            .field("lease_id", &"<redacted>")
            .field("renewal", &self.renewal)
            .field("release", &self.release)
            .field("callback_abort", &self.callback_abort)
            .field(
                "aborted_in_flight_command_ids",
                &self.aborted_in_flight_command_ids,
            )
            .finish()
    }
}

impl fmt::Display for ControlScopeCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut separator = "";
        if self.renewal.is_some() {
            formatter.write_str("lease renewal failed")?;
            separator = "; ";
        }
        if self.callback_abort.is_some() {
            formatter.write_str(separator)?;
            formatter.write_str("callback was aborted after renewal-failure grace")?;
            separator = "; ";
        }
        if self.release.is_some() {
            formatter.write_str(separator)?;
            formatter.write_str("lease release failed")?;
            separator = "; ";
        }
        if !self.aborted_in_flight_command_ids.is_empty() {
            formatter.write_str(separator)?;
            write!(
                formatter,
                "{} command submission(s) remained ambiguous",
                self.aborted_in_flight_command_ids.len()
            )?;
            separator = "; ";
        }
        if separator.is_empty() {
            formatter.write_str("lease cleanup contained no failure")?;
        }
        Ok(())
    }
}

impl std::error::Error for ControlScopeCleanupError {}

/// Failure from acquisition, the callback, or deterministic scoped cleanup.
#[derive(Debug)]
pub enum ControlScopeError<E> {
    /// Exclusive control could not be acquired.
    Acquisition(SdkError),
    /// The callback failed and cleanup completed successfully.
    Operation(E),
    /// The callback succeeded or was aborted, but scoped cleanup failed.
    Cleanup(ControlScopeCleanupError),
    /// Both the callback and renewal/release cleanup failed.
    OperationAndCleanup {
        /// Original callback failure.
        operation: E,
        /// Independently retained cleanup failure evidence.
        cleanup: ControlScopeCleanupError,
    },
}

impl<E> fmt::Display for ControlScopeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acquisition(error) => write!(formatter, "control acquisition failed: {error}"),
            Self::Operation(_) => formatter.write_str("scoped control callback failed"),
            Self::Cleanup(error) => write!(formatter, "scoped control cleanup failed: {error}"),
            Self::OperationAndCleanup { cleanup, .. } => {
                write!(
                    formatter,
                    "scoped control callback and cleanup failed: {cleanup}"
                )
            }
        }
    }
}

impl<E: fmt::Debug> std::error::Error for ControlScopeError<E> {}

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

    /// Runs one callback under an automatically renewed scoped lease.
    ///
    /// Renewal is driven by this future itself—never by a detached task—and
    /// begins only after acquisition succeeds. The callback remains polled
    /// while a renewal exchange is pending. When this returned future is polled
    /// to completion, the callback future is always dropped before the SDK
    /// awaits release, so it cannot retain the capability beyond cleanup.
    ///
    /// A renewal failure fences subsequent [`ScopedControl::submit`] calls.
    /// The callback then receives a grace no longer than
    /// [`MAX_CONTROL_SCOPE_RENEWAL_FAILURE_GRACE`] to stop cooperatively before
    /// it is dropped. Cleanup reports that abort, any pending submission IDs,
    /// the renewal failure, and an independent release failure together.
    ///
    /// Dropping or cancelling this outer future—or unwinding through it after
    /// a callback panic—cannot await network I/O and therefore does **not**
    /// guarantee lease release. Callers needing the normal-completion release
    /// guarantee must drive this future to its `Result`.
    pub async fn with_control<T, E, F>(
        &self,
        ttl_ms: u32,
        operation: F,
    ) -> Result<T, ControlScopeError<E>>
    where
        F: for<'scope> FnOnce(ScopedControl<'scope>) -> ControlScopeFuture<'scope, T, E>,
    {
        if ttl_ms == 0 || ttl_ms > xenoteer_protocol::MAX_LEASE_TTL_MS {
            return Err(ControlScopeError::Acquisition(SdkError::InvalidRequest));
        }
        let mut lease = self
            .acquire_control(Some(ttl_ms))
            .await
            .map_err(ControlScopeError::Acquisition)?;
        let renewal_failed = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(Mutex::new(BTreeMap::new()));
        let control = ScopedControl {
            lease: &lease,
            renewal_failed: Arc::clone(&renewal_failed),
            in_flight: Arc::clone(&in_flight),
        };
        let mut operation_future = operation(control);
        let interval = Duration::from_millis(u64::from((ttl_ms / 3).max(1)));
        let renewal_sleep = tokio::time::sleep(interval);
        tokio::pin!(renewal_sleep);
        let mut renewal_error = None;

        enum CallbackOutcome<T, E> {
            Completed(Result<T, E>),
            Aborted(ControlScopeCallbackAbort),
        }

        let callback_outcome = loop {
            tokio::select! {
                biased;
                result = operation_future.as_mut() => {
                    break CallbackOutcome::Completed(result);
                },
                () = renewal_sleep.as_mut() => {
                    let renewal_future = self.renew_control(lease.id(), Some(ttl_ms));
                    tokio::pin!(renewal_future);
                    let renewal = tokio::select! {
                        biased;
                        result = operation_future.as_mut() => {
                            break CallbackOutcome::Completed(result);
                        },
                        result = renewal_future.as_mut() => result,
                    };
                    let error = match renewal {
                        Ok(state)
                            if state.state == LeaseAvailability::HeldByCaller
                                && state.lease_id == Some(lease.id())
                                && state.expires_at.is_some() => {
                            renewal_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + interval);
                            continue;
                        }
                        Ok(_) => SdkError::InvalidResponse,
                        Err(error) => error,
                    };
                    renewal_failed.store(true, Ordering::Release);
                    renewal_error = Some(error);
                    let grace = interval.min(MAX_CONTROL_SCOPE_RENEWAL_FAILURE_GRACE);
                    let grace_sleep = tokio::time::sleep(grace);
                    tokio::pin!(grace_sleep);
                    break tokio::select! {
                        biased;
                        result = operation_future.as_mut() => {
                            CallbackOutcome::Completed(result)
                        },
                        () = grace_sleep.as_mut() => {
                            CallbackOutcome::Aborted(
                                ControlScopeCallbackAbort::RenewalFailureGraceExpired { grace },
                            )
                        },
                    };
                }
            }
        };
        drop(operation_future);
        let callback_abort = match &callback_outcome {
            CallbackOutcome::Completed(_) => None,
            CallbackOutcome::Aborted(reason) => Some(*reason),
        };
        let aborted_in_flight_command_ids = if callback_abort.is_some() {
            in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        let release_error = lease.release().await.err();
        let cleanup = ControlScopeCleanupError {
            lease_id: lease.id(),
            renewal: renewal_error,
            release: release_error,
            callback_abort,
            aborted_in_flight_command_ids,
        };
        match callback_outcome {
            CallbackOutcome::Completed(operation_result) => {
                match (operation_result, cleanup.is_empty()) {
                    (Ok(value), true) => Ok(value),
                    (Ok(_), false) => Err(ControlScopeError::Cleanup(cleanup)),
                    (Err(error), true) => Err(ControlScopeError::Operation(error)),
                    (Err(operation), false) => {
                        Err(ControlScopeError::OperationAndCleanup { operation, cleanup })
                    }
                }
            }
            CallbackOutcome::Aborted(_) => Err(ControlScopeError::Cleanup(cleanup)),
        }
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

    enum TestLeaseResponse {
        Json(LeaseStateView),
        JsonAndSignal(LeaseStateView, tokio::sync::oneshot::Sender<()>),
        Disconnect,
    }

    async fn serve_lease_responses(
        listener: TcpListener,
        responses: Vec<TestLeaseResponse>,
    ) -> Result<(), std::io::Error> {
        for response in responses {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 8_192];
            let _read = timeout(Duration::from_secs(1), stream.read(&mut request)).await??;
            let (state, completion) = match response {
                TestLeaseResponse::Json(state) => (state, None),
                TestLeaseResponse::JsonAndSignal(state, completion) => (state, Some(completion)),
                TestLeaseResponse::Disconnect => {
                    drop(stream);
                    continue;
                }
            };
            let body = serde_json::to_vec(&state).map_err(std::io::Error::other)?;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            stream.write_all(&body).await?;
            if let Some(completion) = completion {
                let _ignored = completion.send(());
            }
        }
        Ok(())
    }

    fn held_state(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        lease_id: ControlLeaseId,
    ) -> Result<LeaseStateView, TestError> {
        Ok(LeaseStateView {
            desktop_id,
            desktop_generation: generation,
            state: LeaseAvailability::HeldByCaller,
            lease_id: Some(lease_id),
            expires_at: Some(Timestamp::parse("2026-07-30T01:00:00Z")?),
        })
    }

    fn vacant_state(desktop_id: DesktopId, generation: DesktopGeneration) -> LeaseStateView {
        LeaseStateView {
            desktop_id,
            desktop_generation: generation,
            state: LeaseAvailability::Vacant,
            lease_id: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn scoped_control_returns_callback_value_and_awaits_release() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let value = timeout(
            Duration::from_secs(2),
            desktop.with_control(300_000, |control| {
                Box::pin(async move {
                    control.ensure_healthy()?;
                    Ok::<_, SdkError>(42_u8)
                })
            }),
        )
        .await??;
        assert_eq!(value, 42);
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_rejects_invalid_ttl_before_acquisition() -> Result<(), TestError> {
        let desktop = Desktop::for_test(
            Client::new("http://127.0.0.1:9", "lease-test-token-0123456789abcdef")?,
            DesktopId::new(),
            DesktopGeneration::new(),
        );
        let result = desktop
            .with_control(0, |_control| Box::pin(async move { Ok::<_, SdkError>(()) }))
            .await;
        assert!(matches!(
            result,
            Err(ControlScopeError::Acquisition(SdkError::InvalidRequest))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_renews_inside_long_callback_then_releases() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (renewed, renewal_observed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::JsonAndSignal(
                    held_state(desktop_id, generation, lease_id)?,
                    renewed,
                ),
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        timeout(
            Duration::from_secs(2),
            desktop.with_control(60, |control| {
                Box::pin(async move {
                    renewal_observed.await.map_err(|_| SdkError::Transport)?;
                    control.ensure_healthy()
                })
            }),
        )
        .await??;
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn callback_can_complete_while_renewal_exchange_is_pending() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (renewal_started_sender, renewal_started_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut acquire_stream, _) = listener.accept().await?;
            let mut request = [0_u8; 8_192];
            let _read =
                timeout(Duration::from_secs(1), acquire_stream.read(&mut request)).await??;
            let held =
                held_state(desktop_id, generation, lease_id).map_err(std::io::Error::other)?;
            let held_body = serde_json::to_vec(&held).map_err(std::io::Error::other)?;
            acquire_stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        held_body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            acquire_stream.write_all(&held_body).await?;
            drop(acquire_stream);

            let (mut renewal_stream, _) =
                timeout(Duration::from_secs(1), listener.accept()).await??;
            let _read =
                timeout(Duration::from_secs(1), renewal_stream.read(&mut request)).await??;
            let _ignored = renewal_started_sender.send(());

            let (mut release_stream, _) =
                timeout(Duration::from_secs(1), listener.accept()).await??;
            let _read =
                timeout(Duration::from_secs(1), release_stream.read(&mut request)).await??;
            let vacant_body = serde_json::to_vec(&vacant_state(desktop_id, generation))
                .map_err(std::io::Error::other)?;
            release_stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        vacant_body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            release_stream.write_all(&vacant_body).await?;
            drop(renewal_stream);
            Ok::<(), std::io::Error>(())
        });
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let value = timeout(
            Duration::from_millis(500),
            desktop.with_control(30, |_control| {
                Box::pin(async move {
                    renewal_started_receiver
                        .await
                        .map_err(|_| SdkError::Transport)?;
                    Ok::<_, SdkError>(73_u8)
                })
            }),
        )
        .await??;
        assert_eq!(value, 73);
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_preserves_callback_failure_after_successful_release()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(300_000, |_control| {
                Box::pin(async move { Err::<(), _>("callback-failed") })
            }),
        )
        .await?;
        assert!(matches!(
            result,
            Err(ControlScopeError::Operation("callback-failed"))
        ));
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn preobtained_scoped_control_fences_submit_after_renewal_failure_and_still_releases()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Disconnect,
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(30, |control| {
                Box::pin(async move {
                    loop {
                        match control.ensure_healthy() {
                            Ok(()) => tokio::time::sleep(Duration::from_millis(2)).await,
                            Err(SdkError::ControlLeaseRenewalFailed) => {
                                assert!(matches!(
                                    control.submit(Command::DesktopProbe(
                                        xenoteer_protocol::DesktopProbeCommand {},
                                    )),
                                    Err(SdkError::ControlLeaseRenewalFailed)
                                ));
                                return Ok::<_, SdkError>(());
                            }
                            Err(error) => return Err(error),
                        }
                    }
                })
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert!(matches!(cleanup.renewal_error(), Some(SdkError::Transport)));
                assert!(cleanup.release_error().is_none());
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_aborts_noncooperative_callback_after_renewal_failure_and_releases()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Disconnect,
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(30, |_control| {
                Box::pin(std::future::pending::<Result<(), SdkError>>())
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert!(matches!(cleanup.renewal_error(), Some(SdkError::Transport)));
                assert!(cleanup.release_error().is_none());
                assert!(matches!(
                    cleanup.callback_abort(),
                    Some(ControlScopeCallbackAbort::RenewalFailureGraceExpired { .. })
                ));
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn renewal_abort_retains_the_exact_in_flight_command_id() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut acquire_stream, _) = listener.accept().await?;
            let mut request = [0_u8; 8_192];
            let _read =
                timeout(Duration::from_secs(1), acquire_stream.read(&mut request)).await??;
            let held =
                held_state(desktop_id, generation, lease_id).map_err(std::io::Error::other)?;
            let held_body = serde_json::to_vec(&held).map_err(std::io::Error::other)?;
            acquire_stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        held_body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            acquire_stream.write_all(&held_body).await?;
            drop(acquire_stream);

            let (mut command_stream, _) =
                timeout(Duration::from_secs(1), listener.accept()).await??;
            let _read =
                timeout(Duration::from_secs(1), command_stream.read(&mut request)).await??;

            let (mut renewal_stream, _) =
                timeout(Duration::from_secs(1), listener.accept()).await??;
            let _read =
                timeout(Duration::from_secs(1), renewal_stream.read(&mut request)).await??;
            drop(renewal_stream);

            let (mut release_stream, _) =
                timeout(Duration::from_secs(1), listener.accept()).await??;
            let _read =
                timeout(Duration::from_secs(1), release_stream.read(&mut request)).await??;
            let vacant_body = serde_json::to_vec(&vacant_state(desktop_id, generation))
                .map_err(std::io::Error::other)?;
            release_stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        vacant_body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            release_stream.write_all(&vacant_body).await?;
            drop(command_stream);
            Ok::<(), std::io::Error>(())
        });
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let submitted_id = std::sync::Arc::new(std::sync::Mutex::new(None));
        let callback_id = std::sync::Arc::clone(&submitted_id);
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(30, move |control| {
                Box::pin(async move {
                    let submission = control.submit(Command::DesktopProbe(
                        xenoteer_protocol::DesktopProbeCommand {},
                    ))?;
                    *callback_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(submission.id());
                    submission.send().await?;
                    Ok::<_, SdkError>(())
                })
            }),
        )
        .await?;
        let expected_id = *submitted_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(expected_id) = expected_id else {
            return Err("callback did not prepare its command ID".into());
        };
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert_eq!(cleanup.aborted_in_flight_command_ids(), &[expected_id]);
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_retains_renewal_abort_and_release_failures_together()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Disconnect,
                TestLeaseResponse::Disconnect,
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(30, |_control| {
                Box::pin(std::future::pending::<Result<(), SdkError>>())
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert!(matches!(cleanup.renewal_error(), Some(SdkError::Transport)));
                assert!(matches!(cleanup.release_error(), Some(SdkError::Transport)));
                assert!(matches!(
                    cleanup.callback_abort(),
                    Some(ControlScopeCallbackAbort::RenewalFailureGraceExpired { .. })
                ));
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_scoped_control_after_acquisition_cannot_await_release()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (server_callback_started_sender, server_callback_started_receiver) =
            tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 8_192];
            let _read = timeout(Duration::from_secs(1), stream.read(&mut request)).await??;
            let held =
                held_state(desktop_id, generation, lease_id).map_err(std::io::Error::other)?;
            let body = serde_json::to_vec(&held).map_err(std::io::Error::other)?;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            stream.write_all(&body).await?;
            timeout(Duration::from_secs(1), server_callback_started_receiver)
                .await?
                .map_err(std::io::Error::other)?;

            match timeout(Duration::from_millis(100), listener.accept()).await {
                Err(_) => Ok::<(), std::io::Error>(()),
                Ok(Ok(_)) => Err(std::io::Error::other(
                    "cancelled scope unexpectedly sent a release request",
                )),
                Ok(Err(error)) => Err(error),
            }
        });
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let (callback_started_sender, callback_started_receiver) = tokio::sync::oneshot::channel();
        let scope = tokio::spawn(async move {
            desktop
                .with_control(300_000, |_control| {
                    Box::pin(async move {
                        let _ignored = callback_started_sender.send(());
                        let _ignored = server_callback_started_sender.send(());
                        std::future::pending::<Result<(), SdkError>>().await
                    })
                })
                .await
        });
        timeout(Duration::from_secs(1), callback_started_receiver).await??;
        scope.abort();
        let join_error = match timeout(Duration::from_secs(1), scope).await? {
            Err(error) => error,
            Ok(result) => {
                return Err(format!(
                    "aborted scoped-control task unexpectedly completed: {result:?}"
                )
                .into());
            }
        };
        assert!(join_error.is_cancelled());
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_rejects_invalid_renewal_response_and_still_releases()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
                TestLeaseResponse::Json(vacant_state(desktop_id, generation)),
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(30, |control| {
                Box::pin(async move {
                    loop {
                        match control.ensure_healthy() {
                            Ok(()) => tokio::time::sleep(Duration::from_millis(2)).await,
                            Err(SdkError::ControlLeaseRenewalFailed) => {
                                return Ok::<_, SdkError>(());
                            }
                            Err(error) => return Err(error),
                        }
                    }
                })
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert!(matches!(
                    cleanup.renewal_error(),
                    Some(SdkError::InvalidResponse)
                ));
                assert!(cleanup.release_error().is_none());
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_reports_release_failure_without_detaching_cleanup()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Disconnect,
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(300_000, |_control| {
                Box::pin(async move { Ok::<_, SdkError>(()) })
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::Cleanup(cleanup)) => {
                assert!(cleanup.renewal_error().is_none());
                assert!(matches!(cleanup.release_error(), Some(SdkError::Transport)));
                assert_eq!(cleanup.lease_id(), lease_id);
                let lease_id_text = lease_id.to_string();
                assert!(!format!("{cleanup:?}").contains(&lease_id_text));
                assert!(!cleanup.to_string().contains(&lease_id_text));
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_control_retains_callback_and_release_failures_together() -> Result<(), TestError>
    {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = ControlLeaseId::new();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(serve_lease_responses(
            listener,
            vec![
                TestLeaseResponse::Json(held_state(desktop_id, generation, lease_id)?),
                TestLeaseResponse::Disconnect,
            ],
        ));
        let desktop = Desktop::for_test(
            Client::new(base, "lease-test-token-0123456789abcdef")?,
            desktop_id,
            generation,
        );
        let result = timeout(
            Duration::from_secs(2),
            desktop.with_control(300_000, |_control| {
                Box::pin(async move { Err::<(), _>("callback-failed") })
            }),
        )
        .await?;
        match result {
            Err(ControlScopeError::OperationAndCleanup { operation, cleanup }) => {
                assert_eq!(operation, "callback-failed");
                assert!(cleanup.renewal_error().is_none());
                assert!(matches!(cleanup.release_error(), Some(SdkError::Transport)));
            }
            other => return Err(format!("unexpected scoped-control result: {other:?}").into()),
        }
        timeout(Duration::from_secs(1), server).await???;
        Ok(())
    }

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
