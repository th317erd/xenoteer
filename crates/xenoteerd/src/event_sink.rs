//! Startup-safe bridge from desktop observation events to coordinator ingress.

use std::{collections::VecDeque, sync::Mutex};

use thiserror::Error;
use xenoteer_protocol::NormalizedEvent;

use crate::{
    control_plane::{ExternalEventIngress, ExternalEventIngressError},
    observation_plane::{WindowEventSink, WindowEventSinkError},
};

const PREBIND_EVENT_CAPACITY: usize = 256;

trait BoundEventIngress: Send + Sync + 'static {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError>;
    fn require_resync(&self);
}

struct CoordinatorIngress(ExternalEventIngress);

impl BoundEventIngress for CoordinatorIngress {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        self.0
            .try_broadcast(event.topic, event.payload)
            .map_err(|error| match error {
                ExternalEventIngressError::InvalidEvent => WindowEventSinkError::Invalid,
                ExternalEventIngressError::Full => WindowEventSinkError::Full,
                ExternalEventIngressError::Closed => WindowEventSinkError::Closed,
            })
    }

    fn require_resync(&self) {
        self.0.require_resync();
    }
}

struct DeferredState {
    ingress: Option<std::sync::Arc<dyn BoundEventIngress>>,
    binding: bool,
    pending: VecDeque<NormalizedEvent>,
    resync_required: bool,
}

/// Bounded one-time bridge that breaks observation/coordinator startup order.
///
/// Observation emits its initial model-rebuilt event before the coordinator
/// can exist because the coordinator's clipboard runtime needs observation.
/// This bridge retains a bounded startup batch, then drains it after one bind.
/// Any ambiguity discards the batch and publishes a global resync barrier.
pub(crate) struct DeferredWindowEventSink {
    state: Mutex<DeferredState>,
}

impl DeferredWindowEventSink {
    /// Creates an enabled, bounded pre-bind sink.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(DeferredState {
                ingress: None,
                binding: false,
                pending: VecDeque::with_capacity(PREBIND_EVENT_CAPACITY),
                resync_required: false,
            }),
        }
    }

    /// Binds exactly one live coordinator ingress and drains startup metadata.
    pub(crate) fn bind(
        &self,
        ingress: ExternalEventIngress,
    ) -> Result<(), DeferredEventSinkBindError> {
        self.bind_inner(std::sync::Arc::new(CoordinatorIngress(ingress)))
    }

    fn bind_inner(
        &self,
        ingress: std::sync::Arc<dyn BoundEventIngress>,
    ) -> Result<(), DeferredEventSinkBindError> {
        let (mut pending, mut resync_required) = {
            let mut state = lock_state(&self.state);
            if state.ingress.is_some() || state.binding {
                return Err(DeferredEventSinkBindError::AlreadyBound);
            }
            state.binding = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.resync_required),
            )
        };
        loop {
            if resync_required {
                ingress.require_resync();
            } else {
                for event in pending {
                    if ingress.try_emit(event).is_err() {
                        resync_required = true;
                        ingress.require_resync();
                        break;
                    }
                }
            }
            let mut state = lock_state(&self.state);
            if resync_required || state.resync_required {
                state.pending.clear();
                state.resync_required = false;
                state.ingress = Some(std::sync::Arc::clone(&ingress));
                state.binding = false;
                return Ok(());
            }
            pending = std::mem::take(&mut state.pending);
            if pending.is_empty() {
                state.ingress = Some(std::sync::Arc::clone(&ingress));
                state.binding = false;
                return Ok(());
            }
            resync_required = false;
        }
    }
}

impl WindowEventSink for DeferredWindowEventSink {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        let ingress = {
            let mut state = lock_state(&self.state);
            if let Some(ingress) = state.ingress.as_ref() {
                Some(std::sync::Arc::clone(ingress))
            } else if state.resync_required || state.pending.len() >= PREBIND_EVENT_CAPACITY {
                state.pending.clear();
                state.resync_required = true;
                return Err(WindowEventSinkError::Full);
            } else {
                state.pending.push_back(event);
                return Ok(());
            }
        };
        ingress.ok_or(WindowEventSinkError::Closed)?.try_emit(event)
    }

    fn require_resync(&self) {
        let ingress = {
            let mut state = lock_state(&self.state);
            state.pending.clear();
            state.resync_required = true;
            state.ingress.as_ref().map(std::sync::Arc::clone)
        };
        if let Some(ingress) = ingress {
            ingress.require_resync();
        }
    }
}

fn lock_state(mutex: &Mutex<DeferredState>) -> std::sync::MutexGuard<'_, DeferredState> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Invalid deferred event-sink lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum DeferredEventSinkBindError {
    /// The bridge already owns a coordinator ingress.
    #[error("normalized desktop event sink was bound more than once")]
    AlreadyBound,
}

#[cfg(test)]
#[path = "event_sink_tests.rs"]
mod tests;
