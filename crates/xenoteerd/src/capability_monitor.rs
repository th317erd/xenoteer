//! Bounded background refresh for live window-manager capability evidence.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use xenoteer_processd::BrokerClient;
use xenoteer_x11::{
    RawWindowManagerCapabilities, WindowControlActorFailureKind, WindowControlActorHandle,
    WindowControlSubmitError,
};

use crate::artifact_service::StoreArtifactService;

const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Trust state attached to one operation backend's latest active probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendCapabilityEvidenceState {
    /// No active probe has completed yet.
    Pending,
    /// The latest active probe completed successfully.
    Current,
    /// A transient failure followed earlier trustworthy evidence.
    Stale,
    /// No trustworthy evidence is currently available.
    Unavailable,
}

/// Cheap evidence for the non-X11 operation backends used by capability
/// projection. Each field is independent so one backend cannot hide another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationBackendSnapshot {
    pub(crate) artifact: BackendCapabilityEvidenceState,
    pub(crate) process: BackendCapabilityEvidenceState,
}

#[derive(Clone)]
pub(crate) struct OperationBackendReader {
    cache: Arc<RwLock<OperationBackendSnapshot>>,
}

impl OperationBackendReader {
    #[must_use]
    pub(crate) fn snapshot(&self) -> OperationBackendSnapshot {
        *read_lock(&self.cache)
    }
}

/// Owned cancellation/join boundary for artifact and processd probes.
pub(crate) struct OperationBackendMonitor {
    cancellation: CancellationToken,
    join: JoinHandle<()>,
}

impl OperationBackendMonitor {
    pub(crate) async fn shutdown(mut self) -> Result<(), OperationBackendMonitorError> {
        self.cancellation.cancel();
        (&mut self.join)
            .await
            .map_err(|_| OperationBackendMonitorError::Panicked)
    }
}

impl Drop for OperationBackendMonitor {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Starts immediate, bounded probes followed by one refresh every five
/// seconds. Probes run concurrently and update independent evidence fields.
pub(crate) fn spawn_operation_backend_monitor(
    artifacts: Arc<StoreArtifactService>,
    process: BrokerClient,
) -> (OperationBackendMonitor, OperationBackendReader) {
    let cache = Arc::new(RwLock::new(OperationBackendSnapshot {
        artifact: BackendCapabilityEvidenceState::Pending,
        process: BackendCapabilityEvidenceState::Pending,
    }));
    let reader = OperationBackendReader {
        cache: Arc::clone(&cache),
    };
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let join = tokio::spawn(async move {
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = task_cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let artifact_probe = Arc::clone(&artifacts);
                    let artifact = tokio::task::spawn_blocking(move || {
                        artifact_probe.probe_backend().map_err(|_| BackendProbeFailure)
                    });
                    let process = tokio::time::timeout(PROBE_TIMEOUT, process.probe());
                    let (artifact, process) = tokio::join!(artifact, process);
                    let artifact = artifact
                        .map_err(|_| BackendProbeFailure)
                        .and_then(|result| result);
                    let process = process
                        .map_err(|_| BackendProbeFailure)
                        .and_then(|result| result.map_err(|_| BackendProbeFailure));
                    apply_operation_probe_results(&cache, artifact, process);
                }
            }
        }
    });
    (OperationBackendMonitor { cancellation, join }, reader)
}

fn apply_operation_probe_results(
    cache: &RwLock<OperationBackendSnapshot>,
    artifact: Result<(), BackendProbeFailure>,
    process: Result<(), BackendProbeFailure>,
) {
    let mut snapshot = write_lock(cache);
    apply_backend_probe_result(&mut snapshot.artifact, artifact);
    apply_backend_probe_result(&mut snapshot.process, process);
}

fn apply_backend_probe_result(
    state: &mut BackendCapabilityEvidenceState,
    result: Result<(), BackendProbeFailure>,
) {
    *state = match result {
        Ok(()) => BackendCapabilityEvidenceState::Current,
        Err(_) if matches!(state, BackendCapabilityEvidenceState::Current) => {
            BackendCapabilityEvidenceState::Stale
        }
        Err(_) if matches!(state, BackendCapabilityEvidenceState::Stale) => {
            BackendCapabilityEvidenceState::Stale
        }
        Err(_) => BackendCapabilityEvidenceState::Unavailable,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackendProbeFailure;

/// Trust state attached to the latest cached capability projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowCapabilityEvidenceState {
    /// No probe has completed yet.
    Pending,
    /// The latest scheduled probe completed successfully.
    Current,
    /// A transient failure followed an earlier trustworthy projection.
    Stale,
    /// No trustworthy projection is currently available.
    Unavailable,
}

/// Cheap snapshot read by the synchronous HTTP capability provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowCapabilitySnapshot {
    pub(crate) evidence_state: WindowCapabilityEvidenceState,
    pub(crate) capabilities: Option<RawWindowManagerCapabilities>,
}

#[derive(Clone)]
pub(crate) struct WindowCapabilityReader {
    cache: Arc<RwLock<WindowCapabilitySnapshot>>,
}

impl WindowCapabilityReader {
    #[must_use]
    pub(crate) fn snapshot(&self) -> WindowCapabilitySnapshot {
        read_lock(&self.cache).clone()
    }
}

/// Owned cancellation/join boundary for the low-frequency probe task.
pub(crate) struct WindowCapabilityMonitor {
    cancellation: CancellationToken,
    join: JoinHandle<()>,
}

impl WindowCapabilityMonitor {
    pub(crate) async fn shutdown(mut self) -> Result<(), WindowCapabilityMonitorError> {
        self.cancellation.cancel();
        (&mut self.join)
            .await
            .map_err(|_| WindowCapabilityMonitorError::Panicked)
    }
}

impl Drop for WindowCapabilityMonitor {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Starts an immediate probe followed by one bounded refresh every five seconds.
pub(crate) fn spawn_window_capability_monitor(
    handle: WindowControlActorHandle,
) -> (WindowCapabilityMonitor, WindowCapabilityReader) {
    let cache = Arc::new(RwLock::new(WindowCapabilitySnapshot {
        evidence_state: WindowCapabilityEvidenceState::Pending,
        capabilities: None,
    }));
    let reader = WindowCapabilityReader {
        cache: Arc::clone(&cache),
    };
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let join = tokio::spawn(async move {
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = task_cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let result = probe_once(handle.clone()).await;
                    apply_probe_result(&cache, result);
                }
            }
        }
    });
    (WindowCapabilityMonitor { cancellation, join }, reader)
}

async fn probe_once(
    handle: WindowControlActorHandle,
) -> Result<RawWindowManagerCapabilities, WindowCapabilityProbeFailure> {
    tokio::task::spawn_blocking(move || {
        let reply = handle.try_capabilities().map_err(|error| match error {
            WindowControlSubmitError::QueueFull => WindowCapabilityProbeFailure::Busy,
            WindowControlSubmitError::Closed => WindowCapabilityProbeFailure::Terminal,
            WindowControlSubmitError::InvalidRequest(_) => WindowCapabilityProbeFailure::Rejected,
        })?;
        reply
            .recv_timeout(PROBE_TIMEOUT)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => WindowCapabilityProbeFailure::TimedOut,
                RecvTimeoutError::Disconnected => WindowCapabilityProbeFailure::Terminal,
            })?
            .map_err(|error| match error.kind {
                WindowControlActorFailureKind::MalformedWindowManagerData
                | WindowControlActorFailureKind::CapabilityProbeFailed
                | WindowControlActorFailureKind::RevalidationRejected
                | WindowControlActorFailureKind::StaleReference => {
                    WindowCapabilityProbeFailure::Rejected
                }
                WindowControlActorFailureKind::ControlQueueFull => {
                    WindowCapabilityProbeFailure::Busy
                }
                WindowControlActorFailureKind::BackendUnavailable
                | WindowControlActorFailureKind::ActorPoisoned
                | WindowControlActorFailureKind::ActorStopped
                | WindowControlActorFailureKind::ActorPanicked => {
                    WindowCapabilityProbeFailure::Terminal
                }
            })
    })
    .await
    .map_err(|_| WindowCapabilityProbeFailure::Terminal)?
}

fn apply_probe_result(
    cache: &RwLock<WindowCapabilitySnapshot>,
    result: Result<RawWindowManagerCapabilities, WindowCapabilityProbeFailure>,
) {
    let mut snapshot = write_lock(cache);
    match result {
        Ok(capabilities) => {
            snapshot.evidence_state = WindowCapabilityEvidenceState::Current;
            snapshot.capabilities = Some(capabilities);
        }
        Err(WindowCapabilityProbeFailure::Terminal) => {
            snapshot.evidence_state = WindowCapabilityEvidenceState::Unavailable;
            snapshot.capabilities = None;
        }
        Err(
            WindowCapabilityProbeFailure::Busy
            | WindowCapabilityProbeFailure::TimedOut
            | WindowCapabilityProbeFailure::Rejected,
        ) => {
            snapshot.evidence_state = if snapshot.capabilities.is_some() {
                WindowCapabilityEvidenceState::Stale
            } else {
                WindowCapabilityEvidenceState::Unavailable
            };
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowCapabilityProbeFailure {
    Busy,
    TimedOut,
    Rejected,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum WindowCapabilityMonitorError {
    #[error("window capability monitor task panicked")]
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum OperationBackendMonitorError {
    #[error("operation backend capability monitor task panicked")]
    Panicked,
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[path = "capability_monitor_tests.rs"]
mod tests;
