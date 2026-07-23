//! Strict, content-free accessibility event normalization and publication.

use std::sync::Arc;

use xenoteer_protocol::{
    ACCESSIBILITY_ELEMENT_CHANGED_TOPIC, ACCESSIBILITY_ELEMENT_CREATED_TOPIC,
    ACCESSIBILITY_ELEMENT_REMOVED_TOPIC, ACCESSIBILITY_RESYNC_REQUIRED_TOPIC, AccessibilityEvent,
    AccessibilityEventDetail, AccessibilityEventKind, AccessibilityRawSource,
    AccessibilityResyncReason, AccessibilityTextEventDetail, AtspiBusName, AtspiObjectPath,
    EventTopic, NormalizedEvent,
};

use crate::{
    accessibility_plane::{
        AccessibilityEventMetadata, AccessibilityIngestEvent, AccessibilityIngestKind,
        AccessibilityIngestSource, AccessibilitySourceResolution,
    },
    observation_plane::WindowEventSink,
};

/// Cloneable nonblocking publisher over the daemon's shared coordinator ingress.
#[derive(Clone)]
pub(crate) struct AccessibilityEventPublisher {
    sink: Arc<dyn WindowEventSink>,
}

impl AccessibilityEventPublisher {
    pub(crate) fn new(sink: Arc<dyn WindowEventSink>) -> Self {
        Self { sink }
    }

    /// Publishes only additive cache births and exact removals.
    ///
    /// A targeted refresh deliberately produces no cache-add event. Its
    /// following object event supplies the truthful semantic event kind.
    pub(crate) fn publish_cache_transition(&self, transition: &AccessibilityIngestEvent) {
        if !self.sink.enabled() {
            return;
        }
        let built = match transition.kind {
            AccessibilityIngestKind::Upserted => {
                build_cache_events(transition, AccessibilityEventKind::CacheAdded)
            }
            AccessibilityIngestKind::Removed | AccessibilityIngestKind::ApplicationInvalidated => {
                build_cache_events(transition, AccessibilityEventKind::CacheRemoved)
            }
            AccessibilityIngestKind::Refreshed
            | AccessibilityIngestKind::Unchanged
            | AccessibilityIngestKind::BootstrapPending
            | AccessibilityIngestKind::Rebuilt
            | AccessibilityIngestKind::ResyncRequired
            | AccessibilityIngestKind::Correlated => Ok(Vec::new()),
        };
        self.publish_built(built);
    }

    /// Publishes one body-free object event after exact mirror source resolution.
    pub(crate) fn publish_object(&self, source: &AccessibilitySourceResolution, kind: &str) {
        if !self.sink.enabled() {
            return;
        }
        self.publish_built(build_object_event(source, kind).map(|event| vec![event]));
    }

    /// Publishes one source-less barrier using the closed protocol reason enum.
    pub(crate) fn publish_resync(
        &self,
        transition: &AccessibilityIngestEvent,
        reason: AccessibilityResyncReason,
    ) {
        if !self.sink.enabled() {
            return;
        }
        self.publish_built(build_resync_event(transition, reason).map(|event| vec![event]));
    }

    /// Latches the shared capacity-independent global barrier.
    pub(crate) fn require_global_resync(&self) {
        self.sink.require_resync();
    }

    fn publish_built(&self, built: Result<Vec<NormalizedEvent>, EventBuildError>) {
        let events = match built {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(?error, "accessibility event normalization failed closed");
                self.sink.require_resync();
                return;
            }
        };
        for event in events {
            if let Err(error) = self.sink.try_emit(event) {
                tracing::warn!(?error, "accessibility event publication required resync");
                // The coordinator ingress itself latches on queue overflow. This
                // call also covers pre-bind overflow, validation rejection, and
                // adapters that fail before reaching that ingress.
                self.sink.require_resync();
                return;
            }
        }
    }
}

fn build_cache_events(
    transition: &AccessibilityIngestEvent,
    kind: AccessibilityEventKind,
) -> Result<Vec<NormalizedEvent>, EventBuildError> {
    if transition.sources.is_empty()
        || (kind == AccessibilityEventKind::CacheAdded && transition.sources.len() != 1)
    {
        return Err(EventBuildError::Shape);
    }
    transition
        .sources
        .iter()
        .map(|source| {
            if kind == AccessibilityEventKind::CacheAdded && source.element.is_none() {
                return Err(EventBuildError::Shape);
            }
            normalize_event(event_from_source(
                transition.desktop_id,
                transition.desktop_generation,
                transition.atspi_generation,
                transition.revision,
                transition.cache_sequence,
                source,
                kind,
                AccessibilityEventDetail::empty(),
            )?)
        })
        .collect()
}

fn build_object_event(
    resolution: &AccessibilitySourceResolution,
    raw_kind: &str,
) -> Result<NormalizedEvent, EventBuildError> {
    let (kind, detail) = object_event_shape(raw_kind, &resolution.source.metadata)?;
    normalize_event(event_from_source(
        resolution.desktop_id,
        resolution.desktop_generation,
        resolution.atspi_generation,
        resolution.revision,
        resolution.cache_sequence,
        &resolution.source,
        kind,
        detail,
    )?)
}

fn build_resync_event(
    transition: &AccessibilityIngestEvent,
    reason: AccessibilityResyncReason,
) -> Result<NormalizedEvent, EventBuildError> {
    if transition.kind != AccessibilityIngestKind::ResyncRequired || !transition.sources.is_empty()
    {
        return Err(EventBuildError::Shape);
    }
    normalize_event(AccessibilityEvent {
        desktop_id: transition.desktop_id,
        desktop_generation: transition.desktop_generation,
        atspi_generation: transition.atspi_generation,
        source: None,
        raw_source: None,
        kind: AccessibilityEventKind::ResyncRequired,
        resync_reason: Some(reason),
        detail: AccessibilityEventDetail::empty(),
        revision: transition.revision,
        cache_sequence: transition.cache_sequence,
        source_stale: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn event_from_source(
    desktop_id: xenoteer_protocol::DesktopId,
    desktop_generation: xenoteer_protocol::DesktopGeneration,
    atspi_generation: xenoteer_protocol::AtspiGeneration,
    revision: xenoteer_protocol::AccessibilityRevision,
    fallback_cache_sequence: u64,
    source: &AccessibilityIngestSource,
    kind: AccessibilityEventKind,
    detail: AccessibilityEventDetail,
) -> Result<AccessibilityEvent, EventBuildError> {
    let element = source.element.clone();
    let source_stale = element.is_none();
    let cache_sequence = element
        .as_ref()
        .map_or(fallback_cache_sequence, |element| element.cache_sequence);
    Ok(AccessibilityEvent {
        desktop_id,
        desktop_generation,
        atspi_generation,
        source: element,
        raw_source: Some(raw_source(&source.raw)?),
        kind,
        resync_reason: None,
        detail,
        revision,
        cache_sequence,
        source_stale,
    })
}

fn normalize_event(event: AccessibilityEvent) -> Result<NormalizedEvent, EventBuildError> {
    event.validate().map_err(|_| EventBuildError::Validation)?;
    let topic = topic_for(event.kind);
    let payload = serde_json::to_value(event).map_err(|_| EventBuildError::Serialization)?;
    NormalizedEvent::new(
        EventTopic::new(topic).map_err(|_| EventBuildError::Validation)?,
        payload,
    )
    .map_err(|_| EventBuildError::Validation)
}

const fn topic_for(kind: AccessibilityEventKind) -> &'static str {
    match kind {
        AccessibilityEventKind::CacheAdded | AccessibilityEventKind::ElementCreated => {
            ACCESSIBILITY_ELEMENT_CREATED_TOPIC
        }
        AccessibilityEventKind::CacheRemoved | AccessibilityEventKind::ElementRemoved => {
            ACCESSIBILITY_ELEMENT_REMOVED_TOPIC
        }
        AccessibilityEventKind::ResyncRequired => ACCESSIBILITY_RESYNC_REQUIRED_TOPIC,
        _ => ACCESSIBILITY_ELEMENT_CHANGED_TOPIC,
    }
}

fn raw_source(
    source: &xenoteer_atspi::ObjectAddress,
) -> Result<AccessibilityRawSource, EventBuildError> {
    Ok(AccessibilityRawSource {
        bus_name: AtspiBusName::new(source.bus_name()).map_err(|_| EventBuildError::Validation)?,
        object_path: AtspiObjectPath::new(source.object_path())
            .map_err(|_| EventBuildError::Validation)?,
    })
}

fn object_event_shape(
    raw_kind: &str,
    metadata: &AccessibilityEventMetadata,
) -> Result<(AccessibilityEventKind, AccessibilityEventDetail), EventBuildError> {
    let mut detail = AccessibilityEventDetail::empty();
    let kind = match raw_kind {
        "focus.changed" => AccessibilityEventKind::FocusChanged,
        "object.property_changed" => AccessibilityEventKind::PropertyChanged,
        "object.state_changed" => AccessibilityEventKind::StateChanged,
        "object.children_changed"
        | "object.model_changed"
        | "object.row_inserted"
        | "object.row_reordered"
        | "object.row_deleted"
        | "object.column_inserted"
        | "object.column_reordered"
        | "object.column_deleted" => AccessibilityEventKind::ChildrenChanged,
        "object.bounds_changed" | "object.text_bounds_changed" => {
            detail.bounds = metadata.bounds;
            AccessibilityEventKind::BoundsChanged
        }
        "object.link_selected" | "object.selection_changed" => {
            AccessibilityEventKind::SelectionChanged
        }
        "object.visible_data_changed" | "object.announcement" => {
            AccessibilityEventKind::VisibleDataChanged
        }
        "object.value_changed" => {
            detail.value = metadata.value;
            AccessibilityEventKind::ValueChanged
        }
        "object.active_descendant_changed" => AccessibilityEventKind::ActiveDescendantChanged,
        "object.attributes_changed" => {
            detail.property = Some("attributes".to_owned());
            AccessibilityEventKind::PropertyChanged
        }
        "object.text_changed" => {
            detail.property = Some("text_metadata".to_owned());
            detail.text = content_free_text_metadata(metadata, false);
            AccessibilityEventKind::PropertyChanged
        }
        "object.text_attributes_changed" => {
            detail.property = Some("text_attributes".to_owned());
            AccessibilityEventKind::PropertyChanged
        }
        "object.text_caret_moved" => {
            detail.text = content_free_text_metadata(metadata, false);
            AccessibilityEventKind::TextCaretMoved
        }
        "object.text_selection_changed" => {
            detail.text = content_free_text_metadata(metadata, true);
            AccessibilityEventKind::TextSelectionChanged
        }
        "window.activated" => AccessibilityEventKind::WindowActivated,
        "window.deactivated" => AccessibilityEventKind::WindowDeactivated,
        "window.created" | "window.desktop_created" => AccessibilityEventKind::WindowCreated,
        "window.closed" | "window.destroyed" | "window.desktop_destroyed" => {
            AccessibilityEventKind::WindowDestroyed
        }
        "window.moved" | "window.resized" => {
            detail.bounds = metadata.bounds;
            AccessibilityEventKind::BoundsChanged
        }
        "window.property_changed"
        | "window.minimized"
        | "window.maximized"
        | "window.restored"
        | "window.reparented"
        | "window.raised"
        | "window.lowered"
        | "window.shaded"
        | "window.unshaded"
        | "window.restyled" => AccessibilityEventKind::PropertyChanged,
        _ => return Err(EventBuildError::UnknownKind),
    };
    Ok((kind, detail))
}

fn content_free_text_metadata(
    metadata: &AccessibilityEventMetadata,
    prefer_selection: bool,
) -> Option<AccessibilityTextEventDetail> {
    let range = if prefer_selection {
        metadata
            .text_selection
            .or_else(|| metadata.caret_offset.map(|start| (start, 0)))
    } else {
        metadata
            .caret_offset
            .map(|start| (start, 0))
            .or(metadata.text_selection)
    }?;
    Some(AccessibilityTextEventDetail {
        start: range.0,
        length: range.1,
        content: None,
        redacted: true,
    })
}

trait EmptyAccessibilityEventDetail {
    fn empty() -> Self;
}

impl EmptyAccessibilityEventDetail for AccessibilityEventDetail {
    fn empty() -> Self {
        Self {
            property: None,
            state: None,
            enabled: None,
            child: None,
            text: None,
            value: None,
            bounds: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum EventBuildError {
    #[error("accessibility event shape was inconsistent")]
    Shape,
    #[error("accessibility object event kind was not in the closed mapping")]
    UnknownKind,
    #[error("accessibility event failed validation")]
    Validation,
    #[error("accessibility event serialization failed")]
    Serialization,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use xenoteer_atspi::ObjectAddress;
    use xenoteer_protocol::{
        AccessibilityIdentityHash, AccessibilityRevision, ApplicationRef, AtspiGeneration,
        DesktopGeneration, DesktopId, ElementRef,
    };

    use super::*;
    use crate::observation_plane::WindowEventSinkError;

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<NormalizedEvent>>,
        resyncs: AtomicUsize,
        fail: AtomicBool,
    }

    impl RecordingSink {
        fn take(&self) -> Vec<NormalizedEvent> {
            std::mem::take(
                &mut *self
                    .events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
    }

    impl WindowEventSink for RecordingSink {
        fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
            if self.fail.load(Ordering::Acquire) {
                return Err(WindowEventSinkError::Full);
            }
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
            Ok(())
        }

        fn require_resync(&self) {
            self.resyncs.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn fixture_source(
        resolved: bool,
    ) -> Result<
        (
            DesktopId,
            DesktopGeneration,
            AtspiGeneration,
            AccessibilityIngestSource,
        ),
        Box<dyn std::error::Error>,
    > {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let atspi_generation = AtspiGeneration::new(1)?;
        let bus = AtspiBusName::new(":1.42")?;
        let app_path = AtspiObjectPath::new("/test/app")?;
        let object_path = AtspiObjectPath::new("/test/object")?;
        let application = ApplicationRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            unique_bus_name: bus.clone(),
            root_object_path: app_path,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
        };
        let object_identity_hash = AccessibilityIdentityHash::new("b".repeat(64))?;
        let element = resolved.then_some(ElementRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            application,
            object_path,
            object_identity_hash,
            cache_sequence: 7,
        });
        Ok((
            desktop_id,
            desktop_generation,
            atspi_generation,
            AccessibilityIngestSource {
                raw: ObjectAddress::new(":1.42", "/test/object")?,
                element,
                metadata: AccessibilityEventMetadata {
                    bounds: None,
                    value: None,
                    caret_offset: Some(9),
                    text_selection: Some((4, 3)),
                },
            },
        ))
    }

    fn transition(
        kind: AccessibilityIngestKind,
        source: AccessibilityIngestSource,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        atspi_generation: AtspiGeneration,
    ) -> Result<AccessibilityIngestEvent, Box<dyn std::error::Error>> {
        Ok(AccessibilityIngestEvent {
            desktop_id,
            desktop_generation,
            kind,
            atspi_generation,
            revision: AccessibilityRevision::new(3)?,
            cache_sequence: 7,
            sources: vec![source],
        })
    }

    #[test]
    fn cache_added_is_reserved_for_true_upsert_and_refresh_is_silent()
    -> Result<(), Box<dyn std::error::Error>> {
        let (desktop_id, desktop_generation, atspi_generation, source) = fixture_source(true)?;
        let sink = Arc::new(RecordingSink::default());
        let publisher = AccessibilityEventPublisher::new(sink.clone());
        publisher.publish_cache_transition(&transition(
            AccessibilityIngestKind::Upserted,
            source.clone(),
            desktop_id,
            desktop_generation,
            atspi_generation,
        )?);
        let events = sink.take();
        assert_eq!(events.len(), 1);
        let event: AccessibilityEvent = serde_json::from_value(events[0].payload.clone())?;
        assert_eq!(event.kind, AccessibilityEventKind::CacheAdded);
        assert_eq!(
            event.source.as_ref().map(|source| source.cache_sequence),
            Some(7)
        );
        assert!(!event.source_stale);

        publisher.publish_cache_transition(&transition(
            AccessibilityIngestKind::Refreshed,
            source,
            desktop_id,
            desktop_generation,
            atspi_generation,
        )?);
        assert!(sink.take().is_empty());
        assert_eq!(sink.resyncs.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[test]
    fn text_metadata_is_content_free_and_raw_only_sources_are_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        let (desktop_id, desktop_generation, atspi_generation, source) = fixture_source(false)?;
        let normalized = build_object_event(
            &AccessibilitySourceResolution {
                desktop_id,
                desktop_generation,
                atspi_generation,
                revision: AccessibilityRevision::new(4)?,
                cache_sequence: 8,
                source,
            },
            "object.text_changed",
        )?;
        let event: AccessibilityEvent = serde_json::from_value(normalized.payload)?;
        assert_eq!(event.kind, AccessibilityEventKind::PropertyChanged);
        assert_eq!(event.detail.property.as_deref(), Some("text_metadata"));
        let text = event.detail.text.ok_or("missing safe text metadata")?;
        assert_eq!((text.start, text.length), (9, 0));
        assert!(text.redacted);
        assert!(text.content.is_none());
        assert!(event.source.is_none());
        assert!(event.raw_source.is_some());
        assert!(event.source_stale);
        Ok(())
    }

    #[test]
    fn resync_reason_is_closed_and_source_less() -> Result<(), Box<dyn std::error::Error>> {
        let (desktop_id, desktop_generation, atspi_generation, _) = fixture_source(true)?;
        let normalized = build_resync_event(
            &AccessibilityIngestEvent {
                desktop_id,
                desktop_generation,
                kind: AccessibilityIngestKind::ResyncRequired,
                atspi_generation,
                revision: AccessibilityRevision::new(5)?,
                cache_sequence: 7,
                sources: Vec::new(),
            },
            AccessibilityResyncReason::EventQueueOverflow,
        )?;
        let event: AccessibilityEvent = serde_json::from_value(normalized.payload)?;
        assert_eq!(event.kind, AccessibilityEventKind::ResyncRequired);
        assert_eq!(
            event.resync_reason,
            Some(AccessibilityResyncReason::EventQueueOverflow)
        );
        assert!(event.source.is_none());
        assert!(event.raw_source.is_none());
        assert!(!event.source_stale);
        Ok(())
    }

    #[test]
    fn unknown_kind_and_downstream_overflow_latch_global_resync()
    -> Result<(), Box<dyn std::error::Error>> {
        let (desktop_id, desktop_generation, atspi_generation, source) = fixture_source(true)?;
        let resolution = AccessibilitySourceResolution {
            desktop_id,
            desktop_generation,
            atspi_generation,
            revision: AccessibilityRevision::new(6)?,
            cache_sequence: 7,
            source,
        };
        let sink = Arc::new(RecordingSink::default());
        let publisher = AccessibilityEventPublisher::new(sink.clone());
        publisher.publish_object(&resolution, "object.unsupported");
        assert_eq!(sink.resyncs.load(Ordering::Acquire), 1);

        sink.fail.store(true, Ordering::Release);
        publisher.publish_object(&resolution, "focus.changed");
        assert_eq!(sink.resyncs.load(Ordering::Acquire), 2);
        Ok(())
    }
}
