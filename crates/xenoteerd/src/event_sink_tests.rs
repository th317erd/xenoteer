use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use xenoteer_protocol::{EventTopic, NormalizedEvent};

use super::*;

#[derive(Default)]
struct FakeState {
    events: Vec<NormalizedEvent>,
    resyncs: usize,
    failure: Option<WindowEventSinkError>,
}

#[derive(Default)]
struct FakeIngress(Mutex<FakeState>);

impl BoundEventIngress for FakeIngress {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(error) = state.failure {
            return Err(error);
        }
        state.events.push(event);
        Ok(())
    }

    fn require_resync(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.resyncs = state.resyncs.saturating_add(1);
    }
}

struct ReentrantIngress {
    sink: Arc<DeferredWindowEventSink>,
    injected: AtomicBool,
    events: Mutex<Vec<NormalizedEvent>>,
}

impl BoundEventIngress for ReentrantIngress {
    fn try_emit(&self, event: NormalizedEvent) -> Result<(), WindowEventSinkError> {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event);
        if !self.injected.swap(true, Ordering::AcqRel) {
            let injected = NormalizedEvent::new(
                EventTopic::new("window.changed").map_err(|_| WindowEventSinkError::Invalid)?,
                serde_json::json!({"index": 99}),
            )
            .map_err(|_| WindowEventSinkError::Invalid)?;
            self.sink.try_emit(injected)?;
        }
        Ok(())
    }

    fn require_resync(&self) {}
}

fn event(index: usize) -> Result<NormalizedEvent, Box<dyn std::error::Error>> {
    Ok(NormalizedEvent::new(
        EventTopic::new("window.changed")?,
        serde_json::json!({"index": index}),
    )?)
}

#[test]
fn drains_prebind_events_in_order() -> Result<(), Box<dyn std::error::Error>> {
    let sink = DeferredWindowEventSink::new();
    assert_eq!(sink.try_emit(event(1)?), Ok(()));
    assert_eq!(sink.try_emit(event(2)?), Ok(()));
    let ingress = Arc::new(FakeIngress::default());
    sink.bind_inner(ingress.clone())?;
    let state = ingress.0.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(state.events, vec![event(1)?, event(2)?]);
    assert_eq!(state.resyncs, 0);
    Ok(())
}

#[test]
fn prebind_overflow_discards_ambiguous_batch_and_resyncs_once()
-> Result<(), Box<dyn std::error::Error>> {
    let sink = DeferredWindowEventSink::new();
    for index in 0..PREBIND_EVENT_CAPACITY {
        assert_eq!(sink.try_emit(event(index)?), Ok(()));
    }
    assert_eq!(
        sink.try_emit(event(PREBIND_EVENT_CAPACITY)?),
        Err(WindowEventSinkError::Full)
    );
    let ingress = Arc::new(FakeIngress::default());
    sink.bind_inner(ingress.clone())?;
    let state = ingress.0.lock().unwrap_or_else(|error| error.into_inner());
    assert!(state.events.is_empty());
    assert_eq!(state.resyncs, 1);
    Ok(())
}

#[test]
fn explicit_prebind_resync_discards_queued_content() -> Result<(), Box<dyn std::error::Error>> {
    let sink = DeferredWindowEventSink::new();
    assert_eq!(sink.try_emit(event(1)?), Ok(()));
    sink.require_resync();
    let ingress = Arc::new(FakeIngress::default());
    sink.bind_inner(ingress.clone())?;
    let state = ingress.0.lock().unwrap_or_else(|error| error.into_inner());
    assert!(state.events.is_empty());
    assert_eq!(state.resyncs, 1);
    Ok(())
}

#[test]
fn downstream_drain_failure_becomes_resync_without_replaying_tail()
-> Result<(), Box<dyn std::error::Error>> {
    let sink = DeferredWindowEventSink::new();
    assert_eq!(sink.try_emit(event(1)?), Ok(()));
    assert_eq!(sink.try_emit(event(2)?), Ok(()));
    let ingress = Arc::new(FakeIngress::default());
    ingress
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .failure = Some(WindowEventSinkError::Full);
    sink.bind_inner(ingress.clone())?;
    let state = ingress.0.lock().unwrap_or_else(|error| error.into_inner());
    assert!(state.events.is_empty());
    assert_eq!(state.resyncs, 1);
    Ok(())
}

#[test]
fn rejects_second_bind_and_forwards_after_first_bind() -> Result<(), Box<dyn std::error::Error>> {
    let sink = DeferredWindowEventSink::new();
    let first = Arc::new(FakeIngress::default());
    sink.bind_inner(first.clone())?;
    assert_eq!(
        sink.bind_inner(Arc::new(FakeIngress::default())),
        Err(DeferredEventSinkBindError::AlreadyBound)
    );
    assert_eq!(sink.try_emit(event(7)?), Ok(()));
    assert_eq!(
        first
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .events,
        vec![event(7)?]
    );
    Ok(())
}

#[test]
fn events_arriving_during_bind_follow_the_startup_batch() -> Result<(), Box<dyn std::error::Error>>
{
    let sink = Arc::new(DeferredWindowEventSink::new());
    assert_eq!(sink.try_emit(event(1)?), Ok(()));
    let ingress = Arc::new(ReentrantIngress {
        sink: Arc::clone(&sink),
        injected: AtomicBool::new(false),
        events: Mutex::new(Vec::new()),
    });
    sink.bind_inner(ingress.clone())?;
    assert_eq!(
        *ingress
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
        vec![event(1)?, event(99)?]
    );
    Ok(())
}
