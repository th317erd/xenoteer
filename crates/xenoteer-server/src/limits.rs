//! Bounded HTTP admission and transport timing policy.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    extract::{Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use xenoteer_protocol::RequestId;

use crate::problem::ApiProblem;

/// Default ordinary JSON and reassembled WebSocket message limit.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1_048_576;
/// Default number of HTTP requests admitted concurrently.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 128;
/// Default number of REST command long polls admitted concurrently.
///
/// This remains below the default concurrent-request limit so long-lived reads
/// cannot consume every HTTP request slot needed by control mutations.
pub const DEFAULT_MAX_CONCURRENT_LONG_POLLS: usize = 64;
/// Default number of REST command long polls admitted for one principal.
pub const DEFAULT_MAX_CONCURRENT_LONG_POLLS_PER_PRINCIPAL: usize = 8;
/// Default transport-handler timeout. Accepted command work must be detached.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// WebSocket hello deadline.
pub const DEFAULT_WS_HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Expected application heartbeat period advertised to clients.
pub const DEFAULT_WS_HEARTBEAT: Duration = Duration::from_secs(15);
/// Session becomes stale after three missed application heartbeats.
pub const DEFAULT_WS_STALE_TIMEOUT: Duration = Duration::from_secs(45);
/// Bounded per-session normal outbound queue.
pub const DEFAULT_WS_OUTBOUND_CAPACITY: usize = 1_024;
/// Reserved per-session queue for terminal results, errors, and close frames.
pub const DEFAULT_WS_HIGH_PRIORITY_CAPACITY: usize = 32;

/// Validated HTTP and WebSocket transport ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportLimits {
    max_body_bytes: usize,
    max_concurrent_requests: usize,
    request_timeout: Duration,
    ws_hello_timeout: Duration,
    ws_heartbeat: Duration,
    ws_stale_timeout: Duration,
    ws_outbound_capacity: usize,
    ws_high_priority_capacity: usize,
}

impl TransportLimits {
    /// Replaces the ordinary body/message byte ceiling.
    pub fn with_max_body_bytes(mut self, value: usize) -> Result<Self, TransportLimitError> {
        if value == 0 || value > DEFAULT_MAX_BODY_BYTES {
            return Err(TransportLimitError::BodyBytes);
        }
        self.max_body_bytes = value;
        Ok(self)
    }

    /// Replaces the HTTP request concurrency ceiling.
    pub fn with_max_concurrent_requests(
        mut self,
        value: usize,
    ) -> Result<Self, TransportLimitError> {
        if value == 0 || value > 4_096 {
            return Err(TransportLimitError::Concurrency);
        }
        self.max_concurrent_requests = value;
        Ok(self)
    }

    /// Replaces the request-handler timeout.
    pub fn with_request_timeout(mut self, value: Duration) -> Result<Self, TransportLimitError> {
        if value.is_zero() || value > Duration::from_secs(300) {
            return Err(TransportLimitError::RequestTimeout);
        }
        self.request_timeout = value;
        Ok(self)
    }

    /// Replaces WebSocket handshake/heartbeat timing as one consistent policy.
    pub fn with_websocket_timing(
        mut self,
        hello: Duration,
        heartbeat: Duration,
        stale: Duration,
    ) -> Result<Self, TransportLimitError> {
        if hello.is_zero()
            || heartbeat.is_zero()
            || stale < heartbeat.saturating_mul(3)
            || stale > Duration::from_secs(300)
        {
            return Err(TransportLimitError::WebSocketTiming);
        }
        self.ws_hello_timeout = hello;
        self.ws_heartbeat = heartbeat;
        self.ws_stale_timeout = stale;
        Ok(self)
    }

    /// Returns the ordinary body/message byte ceiling.
    #[must_use]
    pub const fn max_body_bytes(self) -> usize {
        self.max_body_bytes
    }

    /// Returns the HTTP concurrency ceiling.
    #[must_use]
    pub const fn max_concurrent_requests(self) -> usize {
        self.max_concurrent_requests
    }

    /// Returns the request-handler timeout.
    #[must_use]
    pub const fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    /// Returns the WebSocket hello timeout.
    #[must_use]
    pub const fn ws_hello_timeout(self) -> Duration {
        self.ws_hello_timeout
    }

    /// Returns the advertised application heartbeat period.
    #[must_use]
    pub const fn ws_heartbeat(self) -> Duration {
        self.ws_heartbeat
    }

    /// Returns the session stale timeout.
    #[must_use]
    pub const fn ws_stale_timeout(self) -> Duration {
        self.ws_stale_timeout
    }

    /// Returns the bounded normal outbound queue capacity.
    #[must_use]
    pub const fn ws_outbound_capacity(self) -> usize {
        self.ws_outbound_capacity
    }

    /// Returns the queue capacity reserved for non-droppable session output.
    #[must_use]
    pub const fn ws_high_priority_capacity(self) -> usize {
        self.ws_high_priority_capacity
    }
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            ws_hello_timeout: DEFAULT_WS_HELLO_TIMEOUT,
            ws_heartbeat: DEFAULT_WS_HEARTBEAT,
            ws_stale_timeout: DEFAULT_WS_STALE_TIMEOUT,
            ws_outbound_capacity: DEFAULT_WS_OUTBOUND_CAPACITY,
            ws_high_priority_capacity: DEFAULT_WS_HIGH_PRIORITY_CAPACITY,
        }
    }
}

/// Invalid transport-limit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransportLimitError {
    /// Body limit must be in the release-one 1..=1MiB range.
    #[error("ordinary request body limit must be between 1 and 1048576 bytes")]
    BodyBytes,
    /// HTTP concurrency must be bounded and nonzero.
    #[error("HTTP request concurrency limit must be between 1 and 4096")]
    Concurrency,
    /// Request timeout must be nonzero and at most five minutes.
    #[error("HTTP request timeout must be between 1ns and 300s")]
    RequestTimeout,
    /// WebSocket stale timeout must cover at least three heartbeat periods.
    #[error("WebSocket timing limits are inconsistent")]
    WebSocketTiming,
}

#[derive(Clone)]
pub(crate) struct AdmissionControl {
    limits: TransportLimits,
    concurrent: Arc<Semaphore>,
}

impl AdmissionControl {
    pub(crate) fn new(limits: TransportLimits) -> Self {
        Self {
            limits,
            concurrent: Arc::new(Semaphore::new(limits.max_concurrent_requests())),
        }
    }
}

/// Independent admission for REST command long polls.
///
/// The global semaphore is deliberately smaller than the HTTP semaphore. The
/// principal table contains only principals with live permits, so its size is
/// bounded by the global long-poll ceiling.
#[derive(Clone)]
pub(crate) struct LongPollAdmission {
    global: Arc<Semaphore>,
    principals: Arc<Mutex<BTreeMap<Arc<str>, usize>>>,
    per_principal: usize,
}

impl LongPollAdmission {
    pub(crate) fn new(limits: TransportLimits) -> Self {
        let global = DEFAULT_MAX_CONCURRENT_LONG_POLLS
            .min(limits.max_concurrent_requests().saturating_sub(1));
        Self {
            global: Arc::new(Semaphore::new(global)),
            principals: Arc::new(Mutex::new(BTreeMap::new())),
            per_principal: DEFAULT_MAX_CONCURRENT_LONG_POLLS_PER_PRINCIPAL,
        }
    }

    pub(crate) fn try_acquire(&self, principal: &str) -> Option<LongPollPermit> {
        let global = Arc::clone(&self.global).try_acquire_owned().ok()?;
        let principal: Arc<str> = Arc::from(principal);
        let mut principals = lock_or_recover(&self.principals);
        let active = principals.entry(Arc::clone(&principal)).or_default();
        if *active >= self.per_principal {
            return None;
        }
        *active += 1;
        drop(principals);
        Some(LongPollPermit {
            _global: global,
            principals: Arc::clone(&self.principals),
            principal,
        })
    }
}

/// RAII ownership of both a global and principal-scoped long-poll slot.
pub(crate) struct LongPollPermit {
    _global: OwnedSemaphorePermit,
    principals: Arc<Mutex<BTreeMap<Arc<str>, usize>>>,
    principal: Arc<str>,
}

impl Drop for LongPollPermit {
    fn drop(&mut self) {
        let mut principals = lock_or_recover(&self.principals);
        let Some(active) = principals.get_mut(&self.principal) else {
            debug_assert!(false, "live long-poll permit must have a principal entry");
            return;
        };
        *active = active.saturating_sub(1);
        if *active == 0 {
            principals.remove(&self.principal);
        }
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) async fn enforce(
    State(control): State<AdmissionControl>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    let exact_command_retry = is_command_submission(&request);
    match checked_content_length(request.headers().get_all(header::CONTENT_LENGTH).iter()) {
        Ok(Some(length)) if length > control.limits.max_body_bytes() as u64 => {
            return ApiProblem::payload_too_large(request_id).into_response();
        }
        Err(()) => return ApiProblem::invalid_request(request_id).into_response(),
        Ok(_) => {}
    }

    let permit = match Arc::clone(&control.concurrent).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiProblem::concurrency_exhausted(request_id).into_response(),
    };
    let handled = tokio::time::timeout(control.limits.request_timeout(), next.run(request)).await;
    drop(permit);
    match handled {
        Ok(response) => response,
        Err(_) => ApiProblem::request_timeout(request_id, exact_command_retry).into_response(),
    }
}

fn is_command_submission(request: &Request) -> bool {
    if request.method() != Method::POST {
        return false;
    }
    let mut segments = request
        .uri()
        .path()
        .split('/')
        .filter(|part| !part.is_empty());
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (
            Some("v1"),
            Some("desktops"),
            Some(_),
            Some("commands"),
            None
        )
    )
}

fn checked_content_length<'a>(
    values: impl Iterator<Item = &'a HeaderValue>,
) -> Result<Option<u64>, ()> {
    let mut values = values;
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    value.parse::<u64>().map(Some).map_err(|_| ())
}

fn request_id(request: &Request) -> RequestId {
    request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        middleware,
        routing::get,
    };
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn limit_validation_rejects_unbounded_values() {
        assert_eq!(
            TransportLimits::default().with_max_body_bytes(DEFAULT_MAX_BODY_BYTES + 1),
            Err(TransportLimitError::BodyBytes)
        );
        assert_eq!(
            TransportLimits::default().with_max_concurrent_requests(0),
            Err(TransportLimitError::Concurrency)
        );
        assert_eq!(
            TransportLimits::default().with_websocket_timing(
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(44),
            ),
            Err(TransportLimitError::WebSocketTiming)
        );
    }

    #[test]
    fn content_length_requires_one_canonical_integer() {
        let valid = [HeaderValue::from_static("1048576")];
        assert_eq!(
            checked_content_length(valid.iter()),
            Ok(Some(DEFAULT_MAX_BODY_BYTES as u64))
        );
        let duplicate = [HeaderValue::from_static("1"), HeaderValue::from_static("1")];
        assert_eq!(checked_content_length(duplicate.iter()), Err(()));
        let invalid = [HeaderValue::from_static("1, 2")];
        assert_eq!(checked_content_length(invalid.iter()), Err(()));
    }

    #[test]
    fn one_principal_cannot_monopolize_long_poll_capacity() -> Result<(), Box<dyn std::error::Error>>
    {
        let admission = LongPollAdmission::new(TransportLimits::default());
        let controller = (0..DEFAULT_MAX_CONCURRENT_LONG_POLLS_PER_PRINCIPAL)
            .map(|_| admission.try_acquire("controller"))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| std::io::Error::other("principal capacity ended early"))?;

        assert!(admission.try_acquire("controller").is_none());
        let observer = admission.try_acquire("observer");
        assert!(observer.is_some(), "another principal retains capacity");

        drop(observer);
        drop(controller);
        assert!(lock_or_recover(&admission.principals).is_empty());
        Ok(())
    }

    #[test]
    fn global_long_poll_cap_reserves_http_capacity_for_control_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default();
        let http = AdmissionControl::new(limits);
        let long_polls = LongPollAdmission::new(limits);
        let held = (0..DEFAULT_MAX_CONCURRENT_LONG_POLLS)
            .map(|index| {
                let http_permit = Arc::clone(&http.concurrent).try_acquire_owned().ok()?;
                let wait_permit = long_polls.try_acquire(&format!("principal-{index}"))?;
                Some((http_permit, wait_permit))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| std::io::Error::other("global long-poll capacity ended early"))?;

        assert!(long_polls.try_acquire("overflow").is_none());
        let ordinary_control = Arc::clone(&http.concurrent).try_acquire_owned();
        assert!(
            ordinary_control.is_ok(),
            "long polls must leave HTTP capacity for cancellation and mutation"
        );

        drop(ordinary_control);
        drop(held);
        assert!(lock_or_recover(&long_polls.principals).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn oversized_declared_body_gets_structured_413() -> Result<(), Box<dyn std::error::Error>>
    {
        let limits = TransportLimits::default().with_max_body_bytes(16)?;
        let app =
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(middleware::from_fn_with_state(
                    AdmissionControl::new(limits),
                    enforce,
                ));
        let response = app
            .oneshot(
                Request::get("/")
                    .header(header::CONTENT_LENGTH, "17")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/problem+json"))
        );
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "resource_exhausted");
        Ok(())
    }

    #[tokio::test]
    async fn handler_timeout_gets_structured_504() -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(1))?;
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    "late"
                }),
            )
            .layer(middleware::from_fn_with_state(
                AdmissionControl::new(limits),
                enforce,
            ));
        let response = app.oneshot(Request::get("/").body(Body::empty())?).await?;
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "request_outcome_unknown");
        assert_eq!(body["retry"], "never");
        assert_eq!(body["effect_stage"], "outcome_unknown");
        Ok(())
    }

    #[tokio::test]
    async fn command_submission_timeout_requires_exact_id_retry()
    -> Result<(), Box<dyn std::error::Error>> {
        use axum::routing::post;

        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(1))?;
        let app = Router::new()
            .route(
                "/v1/desktops/{desktop_id}/commands",
                post(|| async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    "late"
                }),
            )
            .layer(middleware::from_fn_with_state(
                AdmissionControl::new(limits),
                enforce,
            ));
        let response = app
            .oneshot(
                Request::post(format!(
                    "/v1/desktops/{}/commands",
                    xenoteer_protocol::DesktopId::new()
                ))
                .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "request_outcome_unknown");
        assert_eq!(body["retry"], "same_command_id");
        assert_eq!(body["effect_stage"], "outcome_unknown");
        Ok(())
    }
}
