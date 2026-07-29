//! Serialized desktop readiness state.

use serde::Serialize;
use tokio::sync::watch;
use xenoteer_protocol::{DesktopGeneration, DesktopState};

/// Coarse externally visible desktop lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopReadiness {
    /// Process composition is still starting.
    Booting,
    /// Required capability probes are running.
    Probing,
    /// Every required capability probe passed.
    Ready,
    /// Every required capability passed, but an optional capability is unavailable.
    Degraded,
    /// New work is refused while shutdown cleanup runs.
    Draining,
    /// Shutdown completed.
    Stopped,
    /// A critical invariant or subsystem failed.
    Failed,
}

impl From<DesktopReadiness> for DesktopState {
    fn from(value: DesktopReadiness) -> Self {
        match value {
            DesktopReadiness::Booting => Self::Booting,
            DesktopReadiness::Probing => Self::Probing,
            DesktopReadiness::Ready => Self::Ready,
            DesktopReadiness::Degraded => Self::Degraded,
            DesktopReadiness::Draining => Self::Draining,
            DesktopReadiness::Stopped => Self::Stopped,
            DesktopReadiness::Failed => Self::Failed,
        }
    }
}

/// Immutable current readiness snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessSnapshot {
    /// Coarse lifecycle state.
    pub state: DesktopReadiness,
    /// Desktop generation, once a session exists.
    pub desktop_generation: Option<DesktopGeneration>,
    /// Stable safe reason code for non-nominal states.
    pub reason_code: Option<String>,
}

impl ReadinessSnapshot {
    /// Creates a snapshot with an optional safe reason code.
    #[must_use]
    pub fn new(
        state: DesktopReadiness,
        desktop_generation: Option<DesktopGeneration>,
        reason_code: Option<impl Into<String>>,
    ) -> Self {
        Self {
            state,
            desktop_generation,
            reason_code: reason_code.map(Into::into),
        }
    }

    /// Returns the truthful state after the Phase-0 listener is bound.
    ///
    /// Phase 0 has no X11, desktop-session, or accessibility capability probes,
    /// so binding the HTTP listener is liveness evidence but never readiness.
    #[must_use]
    pub fn phase0_backend_probes_not_wired() -> Self {
        Self::new(
            DesktopReadiness::Probing,
            None,
            Some("phase0_backend_probes_not_wired"),
        )
    }

    /// Returns whether command admission may claim the desktop is ready.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(
            self.state,
            DesktopReadiness::Ready | DesktopReadiness::Degraded
        )
    }
}

/// Cloneable readiness reader and sole transition interface.
///
/// The desktop supervisor owns normal lifecycle writes. The handle provides
/// revision-safe watch semantics to health and future status handlers.
#[derive(Debug, Clone)]
pub struct ReadinessHandle {
    sender: watch::Sender<ReadinessSnapshot>,
}

impl ReadinessHandle {
    /// Creates readiness state and its watch channel.
    #[must_use]
    pub fn new(initial: ReadinessSnapshot) -> Self {
        let (sender, _) = watch::channel(initial);
        Self { sender }
    }

    /// Atomically publishes a serialized lifecycle transition.
    pub fn transition(&self, snapshot: ReadinessSnapshot) {
        self.sender.send_replace(snapshot);
    }

    /// Publishes a supervisor transition unless shutdown already owns state.
    ///
    /// This closes the race where an in-flight startup probe completes after
    /// the signal path has entered `Draining` and would otherwise advertise the
    /// desktop as probing or ready again.
    pub fn transition_if_not_stopping(&self, snapshot: ReadinessSnapshot) -> bool {
        let mut changed = false;
        self.sender.send_if_modified(|current| {
            if matches!(
                current.state,
                DesktopReadiness::Draining | DesktopReadiness::Stopped
            ) {
                return false;
            }
            *current = snapshot.clone();
            changed = true;
            true
        });
        changed
    }

    /// Returns an immutable clone of current readiness.
    #[must_use]
    pub fn snapshot(&self) -> ReadinessSnapshot {
        self.sender.borrow().clone()
    }

    /// Subscribes to future readiness transitions.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ReadinessSnapshot> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::{DesktopReadiness, ReadinessHandle, ReadinessSnapshot};

    #[test]
    fn supervisor_transition_cannot_regress_shutdown_state() {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Booting,
            None,
            Some("test_boot"),
        ));
        readiness.transition(ReadinessSnapshot::new(
            DesktopReadiness::Draining,
            None,
            Some("test_draining"),
        ));
        assert!(
            !readiness.transition_if_not_stopping(ReadinessSnapshot::new(
                DesktopReadiness::Ready,
                None,
                None::<String>,
            ))
        );
        assert_eq!(readiness.snapshot().state, DesktopReadiness::Draining);
    }

    #[test]
    fn optional_degradation_preserves_required_readiness() {
        let snapshot = ReadinessSnapshot::new(
            DesktopReadiness::Degraded,
            Some(xenoteer_protocol::DesktopGeneration::new()),
            Some("optional_viewer_unavailable"),
        );
        assert!(snapshot.is_ready());
    }
}
