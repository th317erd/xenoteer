//! Serialized desktop readiness state.

use serde::Serialize;
use tokio::sync::watch;
use xenoteer_protocol::DesktopGeneration;

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
    /// The daemon is alive but a required capability is unavailable.
    Degraded,
    /// New work is refused while shutdown cleanup runs.
    Draining,
    /// Shutdown completed.
    Stopped,
    /// A critical invariant or subsystem failed.
    Failed,
}

/// Immutable current readiness snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessSnapshot {
    /// Coarse lifecycle state.
    pub state: DesktopReadiness,
    /// Desktop generation, once a session exists.
    pub desktop_generation: Option<DesktopGeneration>,
    /// Stable safe reason code for non-ready states.
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
        matches!(self.state, DesktopReadiness::Ready)
    }
}

/// Cloneable readiness reader and sole transition interface.
///
/// The full supervisor actor becomes the sole writer in a later phase. This
/// Phase-0 handle provides the same revision-safe watch semantics to health
/// handlers without implying that desktop probes already exist.
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
