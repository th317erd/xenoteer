use std::sync::Mutex;

use uuid::Uuid;
use xenoteer_protocol::SelectionName;
use xenoteer_x11::ClipboardActorFailureKind;

use super::*;
use crate::observation_plane::WindowEventSinkError;

#[derive(Default)]
struct FakeSink {
    events: Mutex<Vec<NormalizedEvent>>,
    resyncs: Mutex<usize>,
    failure: Mutex<Option<WindowEventSinkError>>,
}

impl WindowEventSink for FakeSink {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        if let Some(error) = *self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return Err(error);
        }
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
        Ok(())
    }

    fn require_resync(&self) {
        let mut resyncs = self
            .resyncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *resyncs = resyncs.saturating_add(1);
    }
}

fn scope() -> (DesktopId, DesktopGeneration) {
    (
        DesktopId::from_uuid(Uuid::from_u128(1)),
        DesktopGeneration::from_uuid(Uuid::from_u128(2)),
    )
}

#[test]
fn emits_valid_content_free_owner_transition() -> Result<(), Box<dyn std::error::Error>> {
    let sink = FakeSink::default();
    let (desktop_id, generation) = scope();
    process_clipboard_event(
        ClipboardActorEvent::OwnershipChanged {
            selection: SelectionName::Clipboard,
            revision: 7,
            owned: true,
        },
        &sink,
        desktop_id,
        generation,
    );
    let events = sink
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic.as_str(), CLIPBOARD_OWNER_CHANGED_TOPIC);
    let payload: ClipboardOwnerChangedEvent = serde_json::from_value(events[0].payload.clone())?;
    assert_eq!(payload.revision, 7);
    assert!(payload.owned_by_xenoteer);
    assert_eq!(payload.validate(), Ok(()));
    Ok(())
}

#[test]
fn wrapped_revision_and_sink_failure_require_resync() {
    let sink = FakeSink::default();
    let (desktop_id, generation) = scope();
    process_clipboard_event(
        ClipboardActorEvent::OwnershipChanged {
            selection: SelectionName::Primary,
            revision: 0,
            owned: false,
        },
        &sink,
        desktop_id,
        generation,
    );
    assert_eq!(
        *sink
            .resyncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        1
    );
    *sink
        .failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(WindowEventSinkError::Full);
    process_clipboard_event(
        ClipboardActorEvent::OwnershipChanged {
            selection: SelectionName::Primary,
            revision: 1,
            owned: false,
        },
        &sink,
        desktop_id,
        generation,
    );
    assert_eq!(
        *sink
            .resyncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        2
    );
}

#[test]
fn transfer_failure_is_not_broadcast_without_principal_context() {
    let sink = FakeSink::default();
    let (desktop_id, generation) = scope();
    process_clipboard_event(
        ClipboardActorEvent::TransferFailed {
            selection: SelectionName::Clipboard,
            failure: ClipboardActorFailureKind::TransferTimeout,
        },
        &sink,
        desktop_id,
        generation,
    );
    assert!(
        sink.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
}

#[test]
fn raw_event_loss_requires_public_resynchronization() {
    let sink = FakeSink::default();
    let (desktop_id, generation) = scope();
    process_clipboard_event(
        ClipboardActorEvent::ResyncRequired,
        &sink,
        desktop_id,
        generation,
    );
    assert_eq!(
        *sink
            .resyncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        1
    );
    assert!(
        sink.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
}
