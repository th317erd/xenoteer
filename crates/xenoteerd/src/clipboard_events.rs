//! Content-free clipboard-owner event normalization and relay.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use xenoteer_protocol::{
    CLIPBOARD_OWNER_CHANGED_TOPIC, ClipboardOwnerChangedEvent, DesktopGeneration, DesktopId,
    EventTopic, NormalizedEvent,
};
use xenoteer_x11::{ClipboardActorEvent, ClipboardActorEventReceiver};

use crate::observation_plane::WindowEventSink;

const RELAY_IDLE_INTERVAL: Duration = Duration::from_millis(10);
const MAX_EVENTS_PER_TURN: usize = 64;

/// Owned lifecycle for the nonblocking clipboard metadata relay.
pub(crate) struct ClipboardEventRelay {
    cancellation: CancellationToken,
    join: JoinHandle<()>,
}

impl ClipboardEventRelay {
    /// Cancels and joins the relay without waiting for the raw actor to stop.
    pub(crate) async fn shutdown(self) -> Result<(), ClipboardEventRelayError> {
        self.cancellation.cancel();
        self.join
            .await
            .map_err(|_| ClipboardEventRelayError::Panicked)
    }
}

/// Starts a bounded polling relay over the raw actor's nonblocking receiver.
pub(crate) fn spawn_clipboard_event_relay(
    receiver: ClipboardActorEventReceiver,
    sink: Arc<dyn WindowEventSink>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> ClipboardEventRelay {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let join = tokio::spawn(async move {
        run_clipboard_event_relay(
            receiver,
            sink,
            desktop_id,
            desktop_generation,
            task_cancellation,
        )
        .await;
    });
    ClipboardEventRelay { cancellation, join }
}

async fn run_clipboard_event_relay(
    receiver: ClipboardActorEventReceiver,
    sink: Arc<dyn WindowEventSink>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    cancellation: CancellationToken,
) {
    loop {
        if cancellation.is_cancelled() {
            return;
        }
        let mut did_work = false;
        for _ in 0..MAX_EVENTS_PER_TURN {
            match receiver.try_recv() {
                Ok(event) => {
                    did_work = true;
                    process_clipboard_event(event, sink.as_ref(), desktop_id, desktop_generation);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if did_work {
            tokio::task::yield_now().await;
        } else {
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(RELAY_IDLE_INTERVAL) => {}
            }
        }
    }
}

fn process_clipboard_event(
    event: ClipboardActorEvent,
    sink: &dyn WindowEventSink,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) {
    if event == ClipboardActorEvent::ResyncRequired {
        sink.require_resync();
        return;
    }
    let ClipboardActorEvent::OwnershipChanged {
        selection,
        revision,
        owned,
    } = event
    else {
        // Paste observations are principal/content-correlated and travel only
        // through the command result. Transfer failures contain no complete
        // owner transition and therefore cannot update public owner metadata.
        return;
    };
    let payload = ClipboardOwnerChangedEvent {
        desktop_id,
        desktop_generation,
        selection,
        revision,
        owned_by_xenoteer: owned,
    };
    let normalized = payload
        .validate()
        .ok()
        .and_then(|()| serde_json::to_value(payload).ok())
        .and_then(|payload| {
            NormalizedEvent::new(
                EventTopic::new(CLIPBOARD_OWNER_CHANGED_TOPIC).ok()?,
                payload,
            )
            .ok()
        });
    let Some(normalized) = normalized else {
        sink.require_resync();
        return;
    };
    if sink.try_emit(normalized).is_err() {
        sink.require_resync();
    }
}

/// Clipboard event relay task failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum ClipboardEventRelayError {
    /// The async relay task panicked or was aborted.
    #[error("clipboard event relay task panicked")]
    Panicked,
}

#[cfg(test)]
#[path = "clipboard_events_tests.rs"]
mod tests;
