//! Bounded HTTP admission and transport timing policy.

use std::{
    collections::BTreeMap,
    error::Error as _,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use http_body_util::LengthLimitError;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use xenoteer_protocol::{MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS, MAX_WINDOW_WAIT_TIMEOUT_MS, RequestId};

use crate::{control::MAX_COMMAND_WAIT_MS, problem::ApiProblem};

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
/// Bounded time for a completed semantic wait to be validated and serialized.
pub const LONG_POLL_RESPONSE_HEADROOM: Duration = Duration::from_secs(5);
/// Extra separation between inner phase deadlines and the outer permit backstop.
const SEMANTIC_WAIT_BACKSTOP_HEADROOM: Duration = Duration::from_secs(5);
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
    let semantic_wait = semantic_wait_route(&request);
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
    let total_timeout = request_backstop_timeout(semantic_wait, control.limits);
    let handled = tokio::time::timeout(total_timeout, next.run(request)).await;
    drop(permit);
    match handled {
        Ok(response) => response,
        Err(_) if semantic_wait.is_some() => {
            ApiProblem::deadline_before_effect(request_id).into_response()
        }
        Err(_) => ApiProblem::request_timeout(request_id, exact_command_retry).into_response(),
    }
}

pub(crate) async fn enforce_semantic_wait(
    State(limits): State<TransportLimits>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    let Some(semantic_wait) = semantic_wait_route(&request) else {
        return next.run(request).await;
    };
    if matches!(
        semantic_wait,
        SemanticWaitRoute::Window | SemanticWaitRoute::Accessibility
    ) {
        request = match collect_semantic_wait_body(
            request,
            limits.request_timeout(),
            limits.max_body_bytes(),
        )
        .await
        {
            Ok(request) => request,
            Err(BodyCollectionFailure::Timeout) => {
                return ApiProblem::deadline_before_effect(request_id).into_response();
            }
            Err(BodyCollectionFailure::TooLarge) => {
                return ApiProblem::payload_too_large(request_id).into_response();
            }
            Err(BodyCollectionFailure::Invalid) => {
                return ApiProblem::invalid_request(request_id).into_response();
            }
        };
    }
    let handler_timeout = request_handler_timeout(
        Some(semantic_wait),
        request.extensions().get::<SemanticWaitBody>(),
        limits,
    );
    let handled = tokio::time::timeout(handler_timeout, next.run(request)).await;
    match handled {
        Ok(response) => response,
        Err(_) => ApiProblem::deadline_before_effect(request_id).into_response(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticWaitRoute {
    Command,
    Window,
    Accessibility,
}

impl SemanticWaitRoute {
    const fn maximum_timeout_ms(self) -> u32 {
        match self {
            Self::Command => MAX_COMMAND_WAIT_MS,
            Self::Window => MAX_WINDOW_WAIT_TIMEOUT_MS,
            Self::Accessibility => MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS,
        }
    }
}

#[derive(Clone)]
struct SemanticWaitBody(Bytes);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyCollectionFailure {
    Timeout,
    TooLarge,
    Invalid,
}

async fn collect_semantic_wait_body(
    request: Request,
    timeout: Duration,
    limit: usize,
) -> Result<Request, BodyCollectionFailure> {
    let (mut parts, body) = request.into_parts();
    let bytes = tokio::time::timeout(timeout, to_bytes(body, limit))
        .await
        .map_err(|_| BodyCollectionFailure::Timeout)?
        .map_err(|error| {
            if error
                .source()
                .is_some_and(|source| source.is::<LengthLimitError>())
            {
                BodyCollectionFailure::TooLarge
            } else {
                BodyCollectionFailure::Invalid
            }
        })?;
    parts.extensions.insert(SemanticWaitBody(bytes.clone()));
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn semantic_wait_route(request: &Request) -> Option<SemanticWaitRoute> {
    let matched_path = request.extensions().get::<MatchedPath>()?;
    classify_semantic_wait(request.method(), matched_path.as_str())
}

fn classify_semantic_wait(method: &Method, matched_path: &str) -> Option<SemanticWaitRoute> {
    match (method, matched_path) {
        (&Method::GET, "/v1/desktops/{desktop_id}/commands/{command_id}/wait") => {
            Some(SemanticWaitRoute::Command)
        }
        (&Method::POST, "/v1/desktops/{desktop_id}/windows/wait") => {
            Some(SemanticWaitRoute::Window)
        }
        (&Method::POST, "/v1/desktops/{desktop_id}/accessibility/elements/wait") => {
            Some(SemanticWaitRoute::Accessibility)
        }
        _ => None,
    }
}

fn request_backstop_timeout(route: Option<SemanticWaitRoute>, limits: TransportLimits) -> Duration {
    let Some(route) = route else {
        return limits.request_timeout();
    };
    let handler =
        checked_response_headroom(Duration::from_millis(u64::from(route.maximum_timeout_ms())))
            .unwrap_or_else(|| limits.request_timeout());
    let body = if matches!(
        route,
        SemanticWaitRoute::Window | SemanticWaitRoute::Accessibility
    ) {
        limits.request_timeout()
    } else {
        Duration::ZERO
    };
    body.checked_add(handler)
        .and_then(|value| value.checked_add(SEMANTIC_WAIT_BACKSTOP_HEADROOM))
        .unwrap_or_else(|| limits.request_timeout())
}

fn request_handler_timeout(
    route: Option<SemanticWaitRoute>,
    body: Option<&SemanticWaitBody>,
    limits: TransportLimits,
) -> Duration {
    let Some(route) = route else {
        return limits.request_timeout();
    };
    if route == SemanticWaitRoute::Command {
        return checked_response_headroom(Duration::from_millis(u64::from(MAX_COMMAND_WAIT_MS)))
            .unwrap_or_else(|| limits.request_timeout());
    }
    let requested = body.and_then(|body| body_wait_timeout_ms(body.0.as_ref()));
    let Some(requested) = requested.filter(|value| *value <= route.maximum_timeout_ms()) else {
        return limits.request_timeout();
    };
    checked_response_headroom(Duration::from_millis(u64::from(requested)))
        .unwrap_or_else(|| limits.request_timeout())
}

fn body_wait_timeout_ms(body: &[u8]) -> Option<u32> {
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let timeout = body.as_object()?.get("timeout_ms")?.as_u64()?;
    u32::try_from(timeout).ok().filter(|timeout| *timeout != 0)
}

fn checked_response_headroom(timeout: Duration) -> Option<Duration> {
    timeout.checked_add(LONG_POLL_RESPONSE_HEADROOM)
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
        routing::{get, post},
    };
    use futures_util::stream;
    use tower::ServiceExt;

    use super::*;

    fn with_transport_limits(router: Router, limits: TransportLimits) -> Router {
        router
            .layer(middleware::from_fn_with_state(
                limits,
                enforce_semantic_wait,
            ))
            .layer(middleware::from_fn_with_state(
                AdmissionControl::new(limits),
                enforce,
            ))
    }

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
    fn exact_semantic_wait_routes_receive_maximum_plus_response_headroom()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(17))?;
        let window =
            classify_semantic_wait(&Method::POST, "/v1/desktops/{desktop_id}/windows/wait");
        let element = classify_semantic_wait(
            &Method::POST,
            "/v1/desktops/{desktop_id}/accessibility/elements/wait",
        );
        let command = classify_semantic_wait(
            &Method::GET,
            "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
        );

        assert_eq!(
            request_handler_timeout(
                window,
                Some(&SemanticWaitBody(Bytes::from_static(
                    br#"{"timeout_ms":300000}"#,
                ))),
                limits,
            ),
            Duration::from_millis(u64::from(xenoteer_protocol::MAX_WINDOW_WAIT_TIMEOUT_MS))
                + LONG_POLL_RESPONSE_HEADROOM
        );
        assert_eq!(
            request_handler_timeout(
                element,
                Some(&SemanticWaitBody(Bytes::from_static(
                    br#"{"timeout_ms":120000}"#,
                ))),
                limits,
            ),
            Duration::from_millis(u64::from(
                xenoteer_protocol::MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS
            )) + LONG_POLL_RESPONSE_HEADROOM
        );
        assert_eq!(
            request_handler_timeout(command, None, limits),
            Duration::from_secs(35)
        );
        assert_eq!(
            request_handler_timeout(
                window,
                Some(&SemanticWaitBody(Bytes::from_static(
                    br#"{"timeout_ms":1}"#
                ))),
                limits,
            ),
            LONG_POLL_RESPONSE_HEADROOM + Duration::from_millis(1)
        );
        assert_eq!(
            request_backstop_timeout(window, limits),
            Duration::from_millis(17) + Duration::from_secs(310)
        );
        assert_eq!(
            request_backstop_timeout(element, limits),
            Duration::from_millis(17) + Duration::from_secs(130)
        );
        assert_eq!(
            request_backstop_timeout(command, limits),
            Duration::from_secs(40)
        );
        Ok(())
    }

    #[tokio::test]
    async fn enforce_applies_extended_budgets_only_to_exact_semantic_wait_routes()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn delayed_ok() -> &'static str {
            tokio::time::sleep(Duration::from_millis(15)).await;
            "ok"
        }

        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(1))?;
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let generation = xenoteer_protocol::DesktopGeneration::new();
        let command_id = xenoteer_protocol::CommandId::new();
        let window_body = serde_json::json!({
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "target": {
                "type": "selector",
                "selector": {
                    "type": "predicate",
                    "predicate": {"type": "active", "value": true}
                },
                "quantifier": "any"
            },
            "predicate": {"type": "exists"},
            "after_revision": null,
            "timeout_ms": 20
        });
        let element_body = serde_json::json!({
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "target": {
                "type": "selector",
                "selector": {
                    "scope": {"type": "desktop"},
                    "predicates": [],
                    "order": "preorder",
                    "result_index": null
                },
                "quantifier": "any"
            },
            "predicate": {"type": "exists"},
            "after_revision": null,
            "timeout_ms": 20,
            "allow_poll_fallback": true
        });
        let window_request: xenoteer_protocol::WindowWaitRequest =
            serde_json::from_value(window_body.clone())?;
        let element_request: xenoteer_protocol::ElementWaitRequest =
            serde_json::from_value(element_body.clone())?;
        assert!(window_request.validate().is_ok());
        assert!(element_request.validate().is_ok());

        let app = with_transport_limits(
            Router::new()
                .route("/v1/desktops/{desktop_id}/windows/wait", post(delayed_ok))
                .route(
                    "/v1/desktops/{desktop_id}/accessibility/elements/wait",
                    post(delayed_ok),
                )
                .route(
                    "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
                    get(delayed_ok),
                )
                .route("/v1/desktops/{desktop_id}/windows/waits", post(delayed_ok)),
            limits,
        );
        let window_path = format!("/v1/desktops/{desktop_id}/windows/wait?future=1");
        let element_path = format!("/v1/desktops/{desktop_id}/accessibility/elements/wait");
        let command_path =
            format!("/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=%32%30");
        let near_miss_path = format!("/v1/desktops/{desktop_id}/windows/waits");
        let window_body = serde_json::to_vec(&window_body)?;
        let element_body = serde_json::to_vec(&element_body)?;

        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..3 {
                let response = app
                    .clone()
                    .oneshot(
                        Request::post(&window_path)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(window_body.clone()))?,
                    )
                    .await?;
                assert_eq!(response.status(), axum::http::StatusCode::OK);

                let response = app
                    .clone()
                    .oneshot(
                        Request::post(&element_path)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(element_body.clone()))?,
                    )
                    .await?;
                assert_eq!(response.status(), axum::http::StatusCode::OK);

                let response = app
                    .clone()
                    .oneshot(Request::get(&command_path).body(Body::empty())?)
                    .await?;
                assert_eq!(response.status(), axum::http::StatusCode::OK);

                let response = app
                    .clone()
                    .oneshot(
                        Request::post(&near_miss_path)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(window_body.clone()))?,
                    )
                    .await?;
                assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn inner_semantic_deadline_wins_before_outer_maximum_backstop()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default();
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let command_id = xenoteer_protocol::CommandId::new();
        let app = with_transport_limits(
            Router::new()
                .route(
                    "/v1/desktops/{desktop_id}/windows/wait",
                    post(|| async { std::future::pending::<&'static str>().await }),
                )
                .route(
                    "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
                    get(|| async { std::future::pending::<&'static str>().await }),
                ),
            limits,
        );

        let started = tokio::time::Instant::now();
        let response = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/desktops/{desktop_id}/windows/wait"))
                    .body(Body::from(r#"{"timeout_ms":300000}"#))?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            Duration::from_secs(305)
        );
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "deadline_exceeded_before_effect");
        assert_eq!(body["effect_stage"], "none");

        let started = tokio::time::Instant::now();
        let response = app
            .oneshot(
                Request::get(format!(
                    "/v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=30000"
                ))
                .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            Duration::from_secs(35)
        );
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "deadline_exceeded_before_effect");
        assert_eq!(body["effect_stage"], "none");
        Ok(())
    }

    #[test]
    fn semantic_wait_route_classification_fails_closed_for_near_misses()
    -> Result<(), Box<dyn std::error::Error>> {
        let near_misses = [
            (Method::GET, "/v1/desktops/{desktop_id}/windows/wait"),
            (Method::POST, "/v1/desktops/{desktop_id}/windows/wait/"),
            (
                Method::POST,
                "/v1/desktops/{tenant}/{desktop_id}/windows/wait",
            ),
            (Method::POST, "/v1/desktops/{desktop_id}/windows/waits"),
            (
                Method::POST,
                "/v1/desktops/{desktop_id}/windows/wait/{extra}",
            ),
            (
                Method::POST,
                "/v1/desktops/{desktop_id}/accessibility/element/wait",
            ),
            (
                Method::POST,
                "/v1/desktops/{desktop_id}/accessibility/elements/wait/{extra}",
            ),
        ];
        for (method, path) in near_misses {
            assert_eq!(
                classify_semantic_wait(&method, path),
                None,
                "near miss received a long-poll budget: {method} {path}"
            );
        }
        Ok(())
    }

    #[test]
    fn response_headroom_arithmetic_is_checked() {
        assert_eq!(
            checked_response_headroom(Duration::from_secs(300)),
            Some(Duration::from_secs(305))
        );
        assert_eq!(checked_response_headroom(Duration::MAX), None);
    }

    #[test]
    fn malformed_semantic_wait_parameters_retain_the_ordinary_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(17))?;
        let window =
            classify_semantic_wait(&Method::POST, "/v1/desktops/{desktop_id}/windows/wait");
        for body in [
            br#"{}"#.as_slice(),
            br#"{"timeout_ms":0}"#.as_slice(),
            br#"{"timeout_ms":300001}"#.as_slice(),
            br#"{"timeout_ms":true}"#.as_slice(),
            br#"{"timeout_ms":"300000"}"#.as_slice(),
            br#"{"timeout_ms":"#.as_slice(),
        ] {
            assert_eq!(
                request_handler_timeout(
                    window,
                    Some(&SemanticWaitBody(Bytes::copy_from_slice(body))),
                    limits,
                ),
                limits.request_timeout()
            );
        }
        Ok(())
    }

    #[test]
    fn exact_command_wait_route_uses_a_fixed_bounded_backstop()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(17))?;
        let command = classify_semantic_wait(
            &Method::GET,
            "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
        );

        assert_eq!(
            request_handler_timeout(command, None, limits),
            Duration::from_secs(35)
        );
        Ok(())
    }

    #[tokio::test]
    async fn slow_semantic_wait_body_remains_under_the_ordinary_timeout()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default().with_request_timeout(Duration::from_millis(1))?;
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let app = with_transport_limits(
            Router::new().route(
                "/v1/desktops/{desktop_id}/windows/wait",
                post(|| async { "parsed" }),
            ),
            limits,
        );
        let body = Body::from_stream(stream::once(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<Bytes, std::io::Error>(Bytes::from_static(br#"{"timeout_ms":300000}"#))
        }));
        let response = app
            .oneshot(Request::post(format!("/v1/desktops/{desktop_id}/windows/wait")).body(body)?)
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            response.headers().get(header::RETRY_AFTER),
            Some(&HeaderValue::from_static("1"))
        );
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["status"], 504);
        assert_eq!(body["code"], "deadline_exceeded_before_effect");
        assert_eq!(body["title"], "Deadline exceeded before effect");
        assert_eq!(
            body["detail"],
            "The semantic read deadline elapsed without a mutating effect; retrying the read is safe."
        );
        assert_eq!(body["retry"], "after_backoff");
        assert_eq!(body["effect_stage"], "none");
        assert_eq!(body["details"], serde_json::json!({}));
        assert!(
            body["instance"]
                .as_str()
                .is_some_and(|instance| instance.starts_with("urn:xenoteer:request:"))
        );
        let problem: xenoteer_protocol::Problem =
            serde_json::from_slice(&serde_json::to_vec(&body)?)?;
        problem.validate()?;
        Ok(())
    }

    #[tokio::test]
    async fn chunked_semantic_wait_body_over_limit_is_413() -> Result<(), Box<dyn std::error::Error>>
    {
        let limits = TransportLimits::default().with_max_body_bytes(16)?;
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let app = with_transport_limits(
            Router::new().route(
                "/v1/desktops/{desktop_id}/windows/wait",
                post(|| async { "parsed" }),
            ),
            limits,
        );
        let body = Body::from_stream(stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from_static(br#"{"timeout_ms":"#)),
            Ok::<Bytes, std::io::Error>(Bytes::from_static(br#"300000}"#)),
        ]));
        let response = app
            .oneshot(Request::post(format!("/v1/desktops/{desktop_id}/windows/wait")).body(body)?)
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        Ok(())
    }

    #[tokio::test]
    async fn semantic_wait_body_stream_error_is_invalid_request_not_oversize()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default();
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let app = with_transport_limits(
            Router::new().route(
                "/v1/desktops/{desktop_id}/windows/wait",
                post(|| async { "parsed" }),
            ),
            limits,
        );
        let body = Body::from_stream(stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from_static(br#"{"timeout_ms":"#)),
            Err::<Bytes, std::io::Error>(std::io::Error::other("body reset")),
        ]));
        let response = app
            .oneshot(Request::post(format!("/v1/desktops/{desktop_id}/windows/wait")).body(body)?)
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "invalid_request");
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_extended_wait_releases_its_http_permit()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = TransportLimits::default();
        let admission = AdmissionControl::new(limits);
        let observed = admission.clone();
        let desktop_id = xenoteer_protocol::DesktopId::new();
        let app = Router::new()
            .route(
                "/v1/desktops/{desktop_id}/windows/wait",
                post(|| async { std::future::pending::<&'static str>().await }),
            )
            .layer(middleware::from_fn_with_state(
                limits,
                enforce_semantic_wait,
            ))
            .layer(middleware::from_fn_with_state(admission, enforce));
        let request = Request::post(format!("/v1/desktops/{desktop_id}/windows/wait"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"timeout_ms":300000}"#))?;
        let task = tokio::spawn(app.oneshot(request));
        for _ in 0..10 {
            if observed.concurrent.available_permits() == limits.max_concurrent_requests() - 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            observed.concurrent.available_permits(),
            limits.max_concurrent_requests() - 1
        );
        task.abort();
        let _cancelled = task.await;
        assert_eq!(
            observed.concurrent.available_permits(),
            limits.max_concurrent_requests()
        );
        Ok(())
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
