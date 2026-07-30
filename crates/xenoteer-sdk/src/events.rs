//! Bounded WebSocket event streams with explicit continuity outcomes.

use std::{fmt, future::Future, sync::Arc, time::Duration};

use futures_util::{Sink, SinkExt, StreamExt};
use http::{StatusCode, header::AUTHORIZATION};
use serde_json::Value;
use tokio::{net::TcpStream, sync::mpsc, task::AbortHandle, time::Instant};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        Error as WebSocketError, Message, client::IntoClientRequest, protocol::WebSocketConfig,
    },
};
use tokio_util::sync::CancellationToken;
use xenoteer_protocol::{
    ClientHello, DesktopGeneration, DesktopId, ErrorCode, EventResumeStatus, EventResyncReason,
    EventTopic, ProtocolVersion, RequestId, SequencedEvent, VersionRange,
    WebSocketClientDescriptor, WebSocketClientMessage, WebSocketServerMessage,
};

use crate::{Client, Desktop, SafeLogOperation, SafeLogOutcome, SafeLogTransport, SdkError};

/// Default maximum number of undelivered non-terminal event items.
pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 256;
/// Maximum caller-configurable event queue capacity.
pub const MAX_EVENT_QUEUE_CAPACITY: usize = 4_096;

const MIN_HEARTBEAT: Duration = Duration::from_millis(250);
const MAX_HEARTBEAT: Duration = Duration::from_secs(120);
const EVENT_RESERVED_QUEUE_SLOTS: usize = 2;

/// Bounded local delivery policy for one event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventStreamOptions {
    /// Maximum queued ordinary event/diagnostic items. Additional slots are
    /// reserved for one continuity-control item and one explicit terminal reason.
    pub queue_capacity: usize,
}

impl Default for EventStreamOptions {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_EVENT_QUEUE_CAPACITY,
        }
    }
}

/// Why an event stream ended without caller-initiated drop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventStreamCloseReason {
    /// The local bounded consumer queue filled.
    QueueOverflow,
    /// Server history continuity was lost; refresh snapshots before subscribing again.
    ResyncRequired,
    /// A structured server error permanently ended this subscription.
    ServerError(ErrorCode),
    /// The server entered a deliberate draining state.
    ServerDraining,
    /// The peer sent a permanent WebSocket close code.
    PeerClosed(u16),
    /// The configured bounded reconnect attempt budget was exhausted.
    ReconnectExhausted,
    /// A frozen protocol invariant was violated.
    ProtocolViolation,
    /// A subscription-bound message did not match the active subscription.
    InvalidMessage {
        /// The message named this desktop but disclosed a different generation.
        generation_changed: bool,
    },
    /// The shared SDK client was explicitly closed.
    ClientClosed,
}

/// Why an event stream requires authoritative snapshot refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventStreamResyncReason {
    /// The server disclosed a different desktop generation.
    GenerationChanged,
    /// The requested cursor is no longer retained by the server.
    HistoryLost,
    /// The requested cursor was ahead of the server's live edge.
    SequenceAhead,
    /// The server-side subscriber could not keep pace.
    SubscriberLag,
    /// The server's outbound delivery queue lost continuity.
    OutboundBackpressure,
    /// The stream moved backward after a sequence was already admitted.
    SequenceRegression,
}

impl From<EventResyncReason> for EventStreamResyncReason {
    fn from(reason: EventResyncReason) -> Self {
        match reason {
            EventResyncReason::GenerationChanged => Self::GenerationChanged,
            EventResyncReason::HistoryLost => Self::HistoryLost,
            EventResyncReason::SequenceAhead => Self::SequenceAhead,
            EventResyncReason::SubscriberLag => Self::SubscriberLag,
            EventResyncReason::OutboundBackpressure => Self::OutboundBackpressure,
        }
    }
}

/// One typed or forward-compatible event-stream observation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EventStreamItem {
    /// Validated globally sequenced server event.
    Event(SequencedEvent),
    /// Authoritative replay continuity was lost; snapshots must be refreshed.
    ResyncRequired {
        /// Stable server or SDK continuity reason when available.
        reason: Option<EventStreamResyncReason>,
        /// Last sequence known dropped by the server.
        dropped_through: Option<u64>,
        /// Current live edge when disclosed.
        latest_sequence: Option<u64>,
    },
    /// A future top-level server message retained as bounded raw JSON.
    UnknownMessage {
        /// Future message discriminator.
        message_type: String,
        /// Complete bounded forward-compatible value.
        raw: Value,
    },
    /// A known message had an invalid nested shape; the socket remains usable.
    MalformedKnownMessage {
        /// Known discriminator whose affected operation failed.
        message_type: String,
    },
    /// Structured server error not tied to secret material.
    ServerError {
        /// Optional request correlation.
        request_id: Option<RequestId>,
        /// Stable error category.
        code: ErrorCode,
        /// Bounded server-safe detail.
        detail: String,
    },
    /// Final local observation. No item follows this one.
    Closed {
        /// Stable terminal reason.
        reason: EventStreamCloseReason,
    },
}

/// Receiver for one bounded event subscription.
pub struct EventStream {
    receiver: mpsc::Receiver<EventStreamItem>,
    task: AbortHandle,
}

impl EventStream {
    /// Receives the next item. A well-formed supervised termination first emits
    /// [`EventStreamItem::Closed`].
    pub async fn next(&mut self) -> Option<EventStreamItem> {
        self.receiver.recv().await
    }

    /// Returns whether local delivery has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }

    /// Closes local event tasks. No server command or effect is cancelled.
    pub fn close(self) {
        self.task.abort();
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventStream")
            .field("queued", &self.receiver.len())
            .finish_non_exhaustive()
    }
}

impl Desktop {
    /// Subscribes with the default bounded local queue.
    pub async fn events(
        &self,
        topics: Vec<EventTopic>,
        since_sequence: Option<u64>,
    ) -> Result<EventStream, SdkError> {
        self.events_with_options(topics, since_sequence, EventStreamOptions::default())
            .await
    }

    /// Subscribes to exact topics and returns only after the correlated
    /// `events.subscribed` acknowledgement is validated.
    pub async fn events_with_options(
        &self,
        topics: Vec<EventTopic>,
        since_sequence: Option<u64>,
        options: EventStreamOptions,
    ) -> Result<EventStream, SdkError> {
        if topics.len() > xenoteer_protocol::MAX_EVENT_TOPICS
            || topics.iter().any(|topic| topic.validate().is_err())
            || topics
                .iter()
                .enumerate()
                .any(|(index, topic)| topics[..index].contains(topic))
            || !(1..=MAX_EVENT_QUEUE_CAPACITY).contains(&options.queue_capacity)
        {
            return Err(SdkError::InvalidRequest);
        }
        self.transport.ensure_open()?;
        let configuration = EventConfiguration {
            client: self.transport.clone(),
            desktop_id: self.id(),
            desktop_generation: self.generation(),
            protocol: self.protocol(),
            topics: Arc::new(topics),
            cancellation: self.transport.cancellation_token(),
        };
        let (socket, heartbeat, subscription_request_id) =
            connect(&configuration, since_sequence, false)
                .await
                .map_err(EventConnectError::into_sdk_error)?;
        let task_guard = self.transport.register_event_task()?;
        let (sender, receiver) = event_channel(options.queue_capacity);
        let task = tokio::spawn(async move {
            let _task_guard = task_guard;
            supervise(
                configuration,
                sender,
                options.queue_capacity,
                socket,
                heartbeat,
                subscription_request_id,
                since_sequence,
            )
            .await;
        });
        Ok(EventStream {
            receiver,
            task: task.abort_handle(),
        })
    }
}

fn event_channel(
    queue_capacity: usize,
) -> (
    mpsc::Sender<EventStreamItem>,
    mpsc::Receiver<EventStreamItem>,
) {
    mpsc::channel(queue_capacity + EVENT_RESERVED_QUEUE_SLOTS)
}

#[derive(Clone)]
pub(crate) struct EventConfiguration {
    pub(crate) client: Client,
    pub(crate) desktop_id: DesktopId,
    pub(crate) desktop_generation: DesktopGeneration,
    pub(crate) protocol: ProtocolVersion,
    pub(crate) topics: Arc<Vec<EventTopic>>,
    pub(crate) cancellation: CancellationToken,
}

#[derive(Debug)]
enum EventConnectError {
    Reconnectable,
    TokenProvider,
    Authentication,
    Permission,
    Server { code: ErrorCode, detail: String },
    Protocol,
}

impl EventConnectError {
    fn into_sdk_error(self) -> SdkError {
        match self {
            Self::Reconnectable => SdkError::Transport,
            Self::TokenProvider => SdkError::TokenProvider,
            Self::Authentication => SdkError::EventHandshakeRejected { status: 401 },
            Self::Permission => SdkError::EventHandshakeRejected { status: 403 },
            Self::Server { code, detail } => SdkError::EventRejected { code, detail },
            Self::Protocol => SdkError::InvalidResponse,
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct ConnectLogGuard {
    client: Client,
    operation: SafeLogOperation,
    succeeded: bool,
}

impl ConnectLogGuard {
    fn new(client: &Client, reconnect: bool) -> Self {
        let operation = if reconnect {
            SafeLogOperation::WebSocketReconnect
        } else {
            SafeLogOperation::WebSocketConnect
        };
        client.safe_log(
            SafeLogTransport::WebSocket,
            operation,
            SafeLogOutcome::Started,
        );
        Self {
            client: client.clone(),
            operation,
            succeeded: false,
        }
    }

    fn succeed(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for ConnectLogGuard {
    fn drop(&mut self) {
        self.client.safe_log(
            SafeLogTransport::WebSocket,
            self.operation,
            if self.succeeded {
                SafeLogOutcome::Succeeded
            } else {
                SafeLogOutcome::Failed
            },
        );
    }
}

async fn connect(
    configuration: &EventConfiguration,
    last_sequence: Option<u64>,
    reconnect: bool,
) -> Result<(Socket, Duration, RequestId), EventConnectError> {
    let mut log = ConnectLogGuard::new(&configuration.client, reconnect);
    let deadline = Instant::now() + configuration.client.connect_timeout();
    configuration
        .client
        .ensure_open()
        .map_err(|_| EventConnectError::Reconnectable)?;
    let mut request = configuration
        .client
        .websocket_url()
        .into_client_request()
        .map_err(|_| EventConnectError::Protocol)?;
    let authorization = until_connect_deadline(
        configuration.client.websocket_authorization_header(),
        &configuration.cancellation,
        deadline,
    )
    .await?
    .map_err(|_| EventConnectError::TokenProvider)?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let websocket_configuration = WebSocketConfig::default()
        .max_message_size(Some(crate::MAX_RESPONSE_BYTES))
        .max_frame_size(Some(crate::MAX_RESPONSE_BYTES));
    let connect = connect_async_tls_with_config(
        request,
        Some(websocket_configuration),
        true,
        Some(Connector::Rustls(configuration.client.tls_config())),
    );
    let (mut socket, _) = until_connect_deadline(connect, &configuration.cancellation, deadline)
        .await?
        .map_err(classify_connect_error)?;

    let hello = ClientHello {
        message_type: "client.hello".to_owned(),
        request_id: RequestId::new(),
        protocol: VersionRange::exact(configuration.protocol),
        client: WebSocketClientDescriptor {
            name: configuration.client.client_metadata().0.to_owned(),
            version: configuration.client.client_metadata().1.to_owned(),
        },
        // Filtering exists only on events.subscribe; hello resume would briefly
        // create an unauthorized all-topic watch.
        resume: None,
    };
    send_json_until(&mut socket, &hello, &configuration.cancellation, deadline).await?;
    let welcome =
        receive_application_message(&mut socket, &configuration.cancellation, deadline).await?;
    let WebSocketServerMessage::Welcome {
        protocol,
        desktop,
        limits,
        resume,
        ..
    } = welcome
    else {
        return Err(EventConnectError::Protocol);
    };
    if protocol != configuration.protocol
        || desktop.id != configuration.desktop_id
        || desktop.generation != Some(configuration.desktop_generation)
        || limits.max_message_bytes == 0
        || usize::try_from(limits.max_message_bytes)
            .ok()
            .is_none_or(|limit| limit > crate::MAX_RESPONSE_BYTES)
        || Duration::from_millis(u64::from(limits.heartbeat_ms)) < MIN_HEARTBEAT
        || Duration::from_millis(u64::from(limits.heartbeat_ms)) > MAX_HEARTBEAT
        || limits.normal_outbound_capacity == 0
        || limits.reserved_outbound_capacity == 0
        || resume.status != EventResumeStatus::NotRequested
    {
        return Err(EventConnectError::Protocol);
    }
    let subscription_request_id = RequestId::new();
    let subscribe = WebSocketClientMessage::EventsSubscribe {
        request_id: subscription_request_id,
        desktop_id: configuration.desktop_id,
        desktop_generation: configuration.desktop_generation,
        topics: configuration.topics.as_ref().clone(),
        since_sequence: last_sequence,
    };
    send_json_until(
        &mut socket,
        &subscribe,
        &configuration.cancellation,
        deadline,
    )
    .await?;
    loop {
        match receive_application_message(&mut socket, &configuration.cancellation, deadline)
            .await?
        {
            WebSocketServerMessage::EventsSubscribed { request_id, topics }
                if request_id == subscription_request_id
                    && topics.as_slice() == configuration.topics.as_slice() =>
            {
                break;
            }
            WebSocketServerMessage::Error { code, detail, .. } => {
                if !valid_server_detail(&detail) {
                    return Err(EventConnectError::Protocol);
                }
                return Err(match code {
                    ErrorCode::AuthenticationRequired => EventConnectError::Authentication,
                    ErrorCode::PermissionDenied => EventConnectError::Permission,
                    _ => EventConnectError::Server { code, detail },
                });
            }
            WebSocketServerMessage::Pong { .. } => {}
            _ => return Err(EventConnectError::Protocol),
        }
    }
    log.succeed();
    Ok((
        socket,
        Duration::from_millis(u64::from(limits.heartbeat_ms)),
        subscription_request_id,
    ))
}

fn classify_connect_error(error: WebSocketError) -> EventConnectError {
    match error {
        WebSocketError::Http(response) if response.status() == StatusCode::UNAUTHORIZED => {
            EventConnectError::Authentication
        }
        WebSocketError::Http(response) if response.status() == StatusCode::FORBIDDEN => {
            EventConnectError::Permission
        }
        WebSocketError::Http(response) if response.status().is_client_error() => {
            EventConnectError::Protocol
        }
        WebSocketError::Protocol(_) | WebSocketError::Utf8(_) | WebSocketError::Capacity(_) => {
            EventConnectError::Protocol
        }
        _ => EventConnectError::Reconnectable,
    }
}

async fn receive_application_message(
    socket: &mut Socket,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<WebSocketServerMessage, EventConnectError> {
    loop {
        let message = until_connect_deadline(socket.next(), cancellation, deadline)
            .await?
            .ok_or(EventConnectError::Reconnectable)?
            .map_err(classify_connect_error)?;
        match message {
            Message::Text(text) => {
                return serde_json::from_str(text.as_ref())
                    .map_err(|_| EventConnectError::Protocol);
            }
            Message::Ping(payload) => {
                until_connect_deadline(socket.send(Message::Pong(payload)), cancellation, deadline)
                    .await?
                    .map_err(|_| EventConnectError::Reconnectable)?
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                return Err(classify_close(
                    frame.as_ref().map(|value| u16::from(value.code)),
                ));
            }
            Message::Binary(_) | Message::Frame(_) => return Err(EventConnectError::Protocol),
        }
    }
}

async fn until_connect_deadline<F, T>(
    future: F,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<T, EventConnectError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        result = tokio::time::timeout_at(deadline, future) => {
            result.map_err(|_| EventConnectError::Reconnectable)
        }
        () = cancellation.cancelled() => Err(EventConnectError::Reconnectable),
    }
}

fn classify_close(code: Option<u16>) -> EventConnectError {
    match code {
        None | Some(1001 | 1012 | 1013) => EventConnectError::Reconnectable,
        Some(4401) => EventConnectError::Authentication,
        Some(1008 | 4403) => EventConnectError::Permission,
        _ => EventConnectError::Protocol,
    }
}

fn encode_json_message<T: serde::Serialize>(value: &T) -> Result<Message, EventConnectError> {
    let encoded = serde_json::to_string(value).map_err(|_| EventConnectError::Protocol)?;
    if encoded.len() > crate::MAX_RESPONSE_BYTES {
        return Err(EventConnectError::Protocol);
    }
    Ok(Message::Text(encoded.into()))
}

async fn send_json_until<S, T>(
    socket: &mut S,
    value: &T,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), EventConnectError>
where
    S: Sink<Message, Error = WebSocketError> + Unpin,
    T: serde::Serialize,
{
    let message = encode_json_message(value)?;
    until_connect_deadline(socket.send(message), cancellation, deadline)
        .await?
        .map_err(|_| EventConnectError::Reconnectable)
}

async fn send_established_message<S>(
    socket: &mut S,
    message: Message,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<(), EventConnectError>
where
    S: Sink<Message, Error = WebSocketError> + Unpin,
{
    until_connect_deadline(socket.send(message), cancellation, Instant::now() + timeout)
        .await?
        .map_err(|_| EventConnectError::Reconnectable)
}

async fn supervise(
    configuration: EventConfiguration,
    sender: mpsc::Sender<EventStreamItem>,
    queue_capacity: usize,
    socket: Socket,
    mut heartbeat: Duration,
    mut subscription_request_id: RequestId,
    mut last_sequence: Option<u64>,
) {
    let mut socket = Some(socket);
    loop {
        let Some(mut established) = socket.take() else {
            let _terminal = sender.try_send(EventStreamItem::Closed {
                reason: EventStreamCloseReason::ProtocolViolation,
            });
            return;
        };
        match run_socket(
            &configuration,
            &sender,
            queue_capacity,
            &mut established,
            heartbeat,
            subscription_request_id,
            &mut last_sequence,
        )
        .await
        {
            SessionEnd::Terminal(reason) => {
                let _terminal = sender.try_send(EventStreamItem::Closed { reason });
                return;
            }
            SessionEnd::Reconnect if sender.is_closed() => return,
            SessionEnd::Reconnect => {}
        }
        retire_established_socket(
            &mut established,
            &configuration.cancellation,
            configuration.client.connect_timeout(),
        )
        .await;
        drop(established);

        let mut attempt = 0_u32;
        let reconnect_policy = configuration.client.reconnect_policy();
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > reconnect_policy.max_attempts {
                let _terminal = sender.try_send(EventStreamItem::Closed {
                    reason: EventStreamCloseReason::ReconnectExhausted,
                });
                return;
            }
            let jitter_sample = RequestId::new().as_uuid().as_bytes()[0];
            let delay = reconnect_delay(reconnect_policy, attempt, jitter_sample);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = configuration.cancellation.cancelled() => {
                    let _terminal = sender.try_send(EventStreamItem::Closed {
                        reason: EventStreamCloseReason::ClientClosed,
                    });
                    return;
                }
            }
            match connect(&configuration, last_sequence, true).await {
                Ok((new_socket, new_heartbeat, new_subscription_request_id)) => {
                    socket = Some(new_socket);
                    heartbeat = new_heartbeat;
                    subscription_request_id = new_subscription_request_id;
                    break;
                }
                Err(EventConnectError::Reconnectable | EventConnectError::TokenProvider) => {}
                Err(EventConnectError::Authentication) => {
                    emit_terminal_server_error(
                        &sender,
                        ErrorCode::AuthenticationRequired,
                        "event authentication failed",
                    );
                    return;
                }
                Err(EventConnectError::Permission) => {
                    emit_terminal_server_error(
                        &sender,
                        ErrorCode::PermissionDenied,
                        "event permission denied",
                    );
                    return;
                }
                Err(EventConnectError::Server { code, detail }) => {
                    let _item = try_emit(
                        &sender,
                        queue_capacity,
                        EventStreamItem::ServerError {
                            request_id: None,
                            code,
                            detail,
                        },
                    );
                    let _terminal = sender.try_send(EventStreamItem::Closed {
                        reason: EventStreamCloseReason::ServerError(code),
                    });
                    return;
                }
                Err(EventConnectError::Protocol) => {
                    let _terminal = sender.try_send(EventStreamItem::Closed {
                        reason: EventStreamCloseReason::ProtocolViolation,
                    });
                    return;
                }
            }
        }
    }
}

async fn retire_established_socket(
    socket: &mut Socket,
    cancellation: &CancellationToken,
    timeout: Duration,
) {
    tokio::select! {
        _result = tokio::time::timeout(timeout, socket.close(None)) => {}
        () = cancellation.cancelled() => {}
    }
}

fn reconnect_delay(policy: crate::ReconnectPolicy, attempt: u32, jitter_sample: u8) -> Duration {
    let multiplier = 1_u32 << attempt.saturating_sub(1).min(30);
    let base = policy
        .initial_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay);
    let available_jitter = policy.max_delay.saturating_sub(base).min(base / 4);
    let jitter_nanos = available_jitter
        .as_nanos()
        .saturating_mul(u128::from(jitter_sample))
        / u128::from(u8::MAX);
    let jitter = Duration::from_nanos(u64::try_from(jitter_nanos).unwrap_or(u64::MAX));
    base.saturating_add(jitter).min(policy.max_delay)
}

fn emit_terminal_server_error(
    sender: &mpsc::Sender<EventStreamItem>,
    code: ErrorCode,
    detail: &str,
) {
    if sender.capacity() > 1 {
        let _error = sender.try_send(EventStreamItem::ServerError {
            request_id: None,
            code,
            detail: detail.to_owned(),
        });
    }
    let _terminal = sender.try_send(EventStreamItem::Closed {
        reason: EventStreamCloseReason::ServerError(code),
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionEnd {
    Reconnect,
    Terminal(EventStreamCloseReason),
}

async fn run_socket(
    configuration: &EventConfiguration,
    sender: &mpsc::Sender<EventStreamItem>,
    queue_capacity: usize,
    socket: &mut Socket,
    heartbeat: Duration,
    subscription_request_id: RequestId,
    last_sequence: &mut Option<u64>,
) -> SessionEnd {
    let tick = (heartbeat / 2).max(MIN_HEARTBEAT);
    let stale_after = heartbeat.saturating_mul(2).max(MIN_HEARTBEAT * 2);
    let send_timeout = heartbeat.min(configuration.client.connect_timeout());
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    let mut last_read = tokio::time::Instant::now();
    let mut pending_ping: Option<(RequestId, String, tokio::time::Instant)> = None;
    loop {
        tokio::select! {
            () = configuration.cancellation.cancelled() => {
                return SessionEnd::Terminal(EventStreamCloseReason::ClientClosed);
            }
            _ = interval.tick() => {
                let now = tokio::time::Instant::now();
                if now.saturating_duration_since(last_read) >= stale_after
                    || pending_ping.as_ref().is_some_and(|(_, _, sent)| {
                        now.saturating_duration_since(*sent) >= heartbeat
                    })
                {
                    return SessionEnd::Reconnect;
                }
                if pending_ping.is_none() {
                    let request_id = RequestId::new();
                    let nonce = RequestId::new().to_string();
                    let ping = WebSocketClientMessage::Ping {
                        request_id,
                        nonce: nonce.clone(),
                    };
                    let message = match encode_json_message(&ping) {
                        Ok(message) => message,
                        Err(_) => {
                            return SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation);
                        }
                    };
                    if send_established_message(
                        socket,
                        message,
                        &configuration.cancellation,
                        send_timeout,
                    ).await.is_err() {
                        return if configuration.cancellation.is_cancelled() {
                            SessionEnd::Terminal(EventStreamCloseReason::ClientClosed)
                        } else {
                            SessionEnd::Reconnect
                        };
                    }
                    pending_ping = Some((request_id, nonce, now));
                }
            }
            message = socket.next() => {
                let Some(message) = message else { return SessionEnd::Reconnect; };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        return match classify_connect_error(error) {
                            EventConnectError::Reconnectable | EventConnectError::TokenProvider => {
                                SessionEnd::Reconnect
                            }
                            _ => SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation),
                        };
                    }
                };
                last_read = tokio::time::Instant::now();
                match message {
                    Message::Text(text) => {
                        match handle_server_text(
                            configuration,
                            sender,
                            queue_capacity,
                            text.as_ref(),
                            subscription_request_id,
                            last_sequence,
                        ) {
                            Ok(TextOutcome::Continue) => {}
                            Ok(TextOutcome::Pong { request_id, nonce }) => {
                                if pending_ping.as_ref().is_none_or(|(expected_id, expected_nonce, _)| {
                                    *expected_id != request_id || *expected_nonce != nonce
                                }) {
                                    return SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation);
                                }
                                pending_ping = None;
                            }
                            Err(reason) => return SessionEnd::Terminal(reason),
                        }
                    }
                    Message::Ping(payload) => {
                        if send_established_message(
                            socket,
                            Message::Pong(payload),
                            &configuration.cancellation,
                            send_timeout,
                        ).await.is_err() {
                            return if configuration.cancellation.is_cancelled() {
                                SessionEnd::Terminal(EventStreamCloseReason::ClientClosed)
                            } else {
                                SessionEnd::Reconnect
                            };
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        let code = frame.as_ref().map(|value| u16::from(value.code));
                        return match code {
                            None | Some(1001 | 1012 | 1013) => SessionEnd::Reconnect,
                            Some(code) => SessionEnd::Terminal(EventStreamCloseReason::PeerClosed(code)),
                        };
                    }
                    Message::Binary(_) | Message::Frame(_) => {
                        return SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation);
                    }
                }
            }
        }
    }
}

pub(crate) fn handle_server_text(
    configuration: &EventConfiguration,
    sender: &mpsc::Sender<EventStreamItem>,
    queue_capacity: usize,
    text: &str,
    subscription_request_id: RequestId,
    last_sequence: &mut Option<u64>,
) -> Result<TextOutcome, EventStreamCloseReason> {
    let raw: Value =
        serde_json::from_str(text).map_err(|_| EventStreamCloseReason::ProtocolViolation)?;
    let message_type = raw
        .get("type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or(EventStreamCloseReason::ProtocolViolation)?
        .to_owned();
    let decoded = serde_json::from_value::<WebSocketServerMessage>(raw.clone());
    let message = match decoded {
        Ok(message) => message,
        Err(_) if subscription_server_message(&message_type) => {
            return Err(EventStreamCloseReason::InvalidMessage {
                generation_changed: false,
            });
        }
        Err(_) if known_server_message(&message_type) => {
            try_emit(
                sender,
                queue_capacity,
                EventStreamItem::MalformedKnownMessage { message_type },
            )?;
            return Ok(TextOutcome::Continue);
        }
        Err(_) => {
            try_emit(
                sender,
                queue_capacity,
                EventStreamItem::UnknownMessage { message_type, raw },
            )?;
            return Ok(TextOutcome::Continue);
        }
    };
    let item = match message {
        WebSocketServerMessage::Event { request_id, event } => {
            event
                .validate()
                .map_err(|_| EventStreamCloseReason::InvalidMessage {
                    generation_changed: false,
                })?;
            if request_id != subscription_request_id
                || event.desktop_id != configuration.desktop_id
                || (!configuration.topics.is_empty()
                    && !configuration.topics.contains(&event.topic))
            {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: false,
                });
            }
            if event.desktop_generation != configuration.desktop_generation {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: true,
                });
            }
            if let Some(last) = *last_sequence {
                if event.sequence == last {
                    return Ok(TextOutcome::Continue);
                }
                if event.sequence < last {
                    emit_sequence_regression(sender, last)?;
                    return Err(EventStreamCloseReason::ResyncRequired);
                }
            }
            let sequence = event.sequence;
            try_emit(sender, queue_capacity, EventStreamItem::Event(event))?;
            // Advance only after the event is durably admitted to the bounded
            // caller queue. Overflow must retain the last actually delivered
            // cursor so recovery never skips the dropped event.
            *last_sequence = Some(sequence);
            return Ok(TextOutcome::Continue);
        }
        WebSocketServerMessage::EventsResyncRequired {
            request_id,
            desktop_id,
            desktop_generation,
            reason,
            dropped_through,
            latest_sequence,
            ..
        } => {
            if request_id != subscription_request_id || desktop_id != configuration.desktop_id {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: false,
                });
            }
            if desktop_generation != configuration.desktop_generation {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: true,
                });
            }
            try_emit_control(
                sender,
                EventStreamItem::ResyncRequired {
                    reason: Some(reason.into()),
                    dropped_through: Some(dropped_through),
                    latest_sequence: Some(latest_sequence),
                },
            )?;
            // Do not replay from a guessed cursor. The caller must refresh
            // authoritative snapshots and explicitly create a new stream.
            return Err(EventStreamCloseReason::ResyncRequired);
        }
        WebSocketServerMessage::Error {
            request_id,
            code,
            detail,
            ..
        } => {
            if !valid_server_detail(&detail) {
                return Err(EventStreamCloseReason::ProtocolViolation);
            }
            try_emit(
                sender,
                queue_capacity,
                EventStreamItem::ServerError {
                    request_id,
                    code,
                    detail,
                },
            )?;
            return Err(EventStreamCloseReason::ServerError(code));
        }
        WebSocketServerMessage::ServerDraining { .. } => {
            return Err(EventStreamCloseReason::ServerDraining);
        }
        WebSocketServerMessage::Pong { request_id, nonce } => {
            return Ok(TextOutcome::Pong { request_id, nonce });
        }
        WebSocketServerMessage::EventsReplayComplete {
            request_id,
            desktop_id,
            desktop_generation,
            through_sequence,
        } => {
            if request_id != subscription_request_id || desktop_id != configuration.desktop_id {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: false,
                });
            }
            if desktop_generation != configuration.desktop_generation {
                return Err(EventStreamCloseReason::InvalidMessage {
                    generation_changed: true,
                });
            }
            if let Some(last) = last_sequence.filter(|last| through_sequence < *last) {
                emit_sequence_regression(sender, last)?;
                return Err(EventStreamCloseReason::ResyncRequired);
            }
            // The server assigns sequence numbers before authorization/topic
            // filtering, so a replay may contain zero visible events. Preserve
            // its authoritative global boundary for the next reconnect.
            *last_sequence = Some(through_sequence);
            None
        }
        WebSocketServerMessage::EventsSubscribed { .. }
        | WebSocketServerMessage::EventsUnsubscribed { .. }
        | WebSocketServerMessage::Welcome { .. } => {
            return Err(EventStreamCloseReason::ProtocolViolation);
        }
        WebSocketServerMessage::CommandAccepted { .. }
        | WebSocketServerMessage::CommandProgress { .. }
        | WebSocketServerMessage::CommandResult { .. }
        | WebSocketServerMessage::CommandUnwatched { .. }
        | WebSocketServerMessage::LeaseState { .. } => None,
    };
    if let Some(item) = item {
        try_emit(sender, queue_capacity, item)?;
    }
    Ok(TextOutcome::Continue)
}

pub(crate) fn try_emit(
    sender: &mpsc::Sender<EventStreamItem>,
    _queue_capacity: usize,
    item: EventStreamItem,
) -> Result<(), EventStreamCloseReason> {
    // Physical channel slots are reserved for continuity control and terminal
    // reason delivery, independently of ordinary queue saturation.
    if sender.capacity() <= EVENT_RESERVED_QUEUE_SLOTS {
        return Err(EventStreamCloseReason::QueueOverflow);
    }
    sender
        .try_send(item)
        .map_err(|_| EventStreamCloseReason::QueueOverflow)
}

fn try_emit_control(
    sender: &mpsc::Sender<EventStreamItem>,
    item: EventStreamItem,
) -> Result<(), EventStreamCloseReason> {
    if sender.capacity() <= 1 {
        return Err(EventStreamCloseReason::QueueOverflow);
    }
    sender
        .try_send(item)
        .map_err(|_| EventStreamCloseReason::QueueOverflow)
}

fn emit_sequence_regression(
    sender: &mpsc::Sender<EventStreamItem>,
    last_sequence: u64,
) -> Result<(), EventStreamCloseReason> {
    try_emit_control(
        sender,
        EventStreamItem::ResyncRequired {
            reason: Some(EventStreamResyncReason::SequenceRegression),
            dropped_through: None,
            latest_sequence: Some(last_sequence),
        },
    )
}

fn valid_server_detail(detail: &str) -> bool {
    !detail.is_empty()
        && detail.len() <= xenoteer_protocol::MAX_PROBLEM_DETAIL_BYTES
        && !detail.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextOutcome {
    Continue,
    Pong {
        request_id: RequestId,
        nonce: String,
    },
}

fn known_server_message(message_type: &str) -> bool {
    matches!(
        message_type,
        "server.welcome"
            | "server.pong"
            | "command.accepted"
            | "command.progress"
            | "command.result"
            | "command.unwatched"
            | "lease.state"
            | "events.subscribed"
            | "events.unsubscribed"
            | "event"
            | "events.replay_complete"
            | "events.resync_required"
            | "server.draining"
            | "error"
    )
}

fn subscription_server_message(message_type: &str) -> bool {
    matches!(
        message_type,
        "event" | "events.replay_complete" | "events.resync_required"
    )
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    use futures_util::Sink;
    use tokio::{
        net::TcpListener,
        time::{Instant, timeout},
    };
    use tokio_tungstenite::{
        WebSocketStream, accept_async,
        tungstenite::protocol::{CloseFrame, frame::coding::CloseCode},
    };
    use xenoteer_protocol::{
        ConnectionId, EventResyncReason, WelcomeDesktop, WelcomeDesktopState, WelcomeLimits,
        WelcomePrincipal, WelcomeResume,
    };

    use super::*;

    type TestError = Box<dyn Error + Send + Sync>;

    struct StalledPeerSink;

    impl Sink<Message> for StalledPeerSink {
        type Error = WebSocketError;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
    }

    struct ObservedStalledPeerSink {
        polled: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for ObservedStalledPeerSink {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Sink<Message> for ObservedStalledPeerSink {
        type Error = WebSocketError;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.polled.notify_one();
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
    }

    fn configuration(
        base: impl AsRef<str>,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        topics: Vec<EventTopic>,
    ) -> Result<EventConfiguration, SdkError> {
        let client = Client::new(base, "event-test-token-0123456789abcdef")?;
        Ok(EventConfiguration {
            cancellation: client.cancellation_token(),
            client,
            desktop_id,
            desktop_generation,
            protocol: ProtocolVersion::V1_0,
            topics: Arc::new(topics),
        })
    }

    fn welcome(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        max_message_bytes: u32,
    ) -> WebSocketServerMessage {
        WebSocketServerMessage::Welcome {
            protocol: ProtocolVersion::V1_0,
            connection_id: ConnectionId::new(),
            principal: WelcomePrincipal {
                id: "event-test".to_owned(),
                capabilities: vec!["desktop:observe".to_owned()],
            },
            desktop: WelcomeDesktop {
                id: desktop_id,
                generation: Some(desktop_generation),
                state: WelcomeDesktopState::Ready,
            },
            limits: WelcomeLimits {
                max_message_bytes,
                heartbeat_ms: 1_000,
                normal_outbound_capacity: 64,
                reserved_outbound_capacity: 8,
                max_command_watches: 8,
            },
            resume: WelcomeResume {
                status: EventResumeStatus::NotRequested,
            },
        }
    }

    async fn accept_subscription(
        listener: &TcpListener,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        heartbeat_ms: u32,
    ) -> Result<
        (
            WebSocketStream<TcpStream>,
            RequestId,
            Vec<EventTopic>,
            Option<u64>,
        ),
        TestError,
    > {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let Message::Text(hello) = socket.next().await.ok_or("missing hello")?? else {
            return Err("hello was not text".into());
        };
        let hello: ClientHello = serde_json::from_str(hello.as_ref())?;
        if hello.resume.is_some() {
            return Err("hello created an unfiltered resume watch".into());
        }
        let mut server_welcome = welcome(
            desktop_id,
            desktop_generation,
            crate::MAX_RESPONSE_BYTES as u32,
        );
        if let WebSocketServerMessage::Welcome { limits, .. } = &mut server_welcome {
            limits.heartbeat_ms = heartbeat_ms;
        }
        socket
            .send(Message::Text(
                serde_json::to_string(&server_welcome)?.into(),
            ))
            .await?;
        let Message::Text(subscribe) = socket.next().await.ok_or("missing subscribe")?? else {
            return Err("subscribe was not text".into());
        };
        let WebSocketClientMessage::EventsSubscribe {
            request_id,
            desktop_id: actual_id,
            desktop_generation: actual_generation,
            topics,
            since_sequence,
        } = serde_json::from_str(subscribe.as_ref())?
        else {
            return Err("wrong subscribe message".into());
        };
        if actual_id != desktop_id || actual_generation != desktop_generation {
            return Err("subscription used the wrong desktop generation".into());
        }
        socket
            .send(Message::Text(
                serde_json::to_string(&WebSocketServerMessage::EventsSubscribed {
                    request_id,
                    topics: topics.clone(),
                })?
                .into(),
            ))
            .await?;
        Ok((socket, request_id, topics, since_sequence))
    }

    async fn send_event(
        socket: &mut WebSocketStream<TcpStream>,
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        sequence: u64,
        topic: EventTopic,
    ) -> Result<(), TestError> {
        socket
            .send(Message::Text(
                serde_json::to_string(&WebSocketServerMessage::Event {
                    request_id,
                    event: SequencedEvent {
                        desktop_id,
                        desktop_generation,
                        sequence,
                        topic,
                        payload: serde_json::json!({"sequence": sequence}),
                    },
                })?
                .into(),
            ))
            .await?;
        Ok(())
    }

    #[test]
    fn unknown_and_non_subscription_malformed_messages_are_bounded_items() -> Result<(), TestError>
    {
        let configuration = configuration(
            "http://127.0.0.1:8080",
            DesktopId::new(),
            DesktopGeneration::new(),
            Vec::new(),
        )?;
        let (sender, mut receiver) = event_channel(2);
        let request_id = RequestId::new();
        let mut sequence = None;
        handle_server_text(
            &configuration,
            &sender,
            2,
            r#"{"type":"future.notice","bounded":true}"#,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("unknown message ended stream: {reason:?}"))?;
        handle_server_text(
            &configuration,
            &sender,
            2,
            r#"{"type":"command.accepted"}"#,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("malformed message ended stream: {reason:?}"))?;
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::UnknownMessage { .. }
        ));
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::MalformedKnownMessage { .. }
        ));
        Ok(())
    }

    #[test]
    fn event_channel_preserves_control_and_terminal_slots() -> Result<(), TestError> {
        let (sender, mut receiver) = event_channel(1);
        try_emit(
            &sender,
            1,
            EventStreamItem::MalformedKnownMessage {
                message_type: "event".to_owned(),
            },
        )
        .map_err(|reason| format!("first queued item failed: {reason:?}"))?;
        assert_eq!(
            try_emit(
                &sender,
                1,
                EventStreamItem::MalformedKnownMessage {
                    message_type: "event".to_owned(),
                },
            ),
            Err(EventStreamCloseReason::QueueOverflow)
        );
        try_emit_control(
            &sender,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::SequenceRegression),
                dropped_through: None,
                latest_sequence: None,
            },
        )
        .map_err(|reason| format!("reserved control item failed: {reason:?}"))?;
        sender.try_send(EventStreamItem::Closed {
            reason: EventStreamCloseReason::ResyncRequired,
        })?;
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::MalformedKnownMessage { .. }
        ));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::SequenceRegression),
                dropped_through: None,
                latest_sequence: None,
            }
        );
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::ResyncRequired
            }
        );
        Ok(())
    }

    #[test]
    fn public_resync_reason_preserves_every_server_reason_and_sdk_regression()
    -> Result<(), TestError> {
        let cases = [
            (
                EventResyncReason::GenerationChanged,
                EventStreamResyncReason::GenerationChanged,
                "generation_changed",
            ),
            (
                EventResyncReason::HistoryLost,
                EventStreamResyncReason::HistoryLost,
                "history_lost",
            ),
            (
                EventResyncReason::SequenceAhead,
                EventStreamResyncReason::SequenceAhead,
                "sequence_ahead",
            ),
            (
                EventResyncReason::SubscriberLag,
                EventStreamResyncReason::SubscriberLag,
                "subscriber_lag",
            ),
            (
                EventResyncReason::OutboundBackpressure,
                EventStreamResyncReason::OutboundBackpressure,
                "outbound_backpressure",
            ),
        ];
        for (wire, public, encoded) in cases {
            assert_eq!(EventStreamResyncReason::from(wire), public);
            assert_eq!(serde_json::to_value(public)?, encoded);
        }
        assert_eq!(
            serde_json::to_value(EventStreamResyncReason::SequenceRegression)?,
            "sequence_regression"
        );
        Ok(())
    }

    #[test]
    fn resync_is_explicit_and_terminal_without_advancing_for_replay() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            vec![EventTopic::new("window.changed")?],
        )?;
        let request_id = RequestId::new();
        let message = serde_json::to_string(&WebSocketServerMessage::EventsResyncRequired {
            request_id,
            desktop_id,
            desktop_generation,
            reason: EventResyncReason::HistoryLost,
            dropped_through: 40,
            latest_sequence: 42,
        })?;
        let (sender, mut receiver) = event_channel(2);
        let mut sequence = Some(12);
        assert_eq!(
            handle_server_text(
                &configuration,
                &sender,
                2,
                &message,
                request_id,
                &mut sequence,
            ),
            Err(EventStreamCloseReason::ResyncRequired)
        );
        assert_eq!(sequence, Some(12));
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::HistoryLost),
                latest_sequence: Some(42),
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn replay_completion_advances_filtered_cursor_and_rejects_regression() -> Result<(), TestError>
    {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            vec![EventTopic::new("window.changed")?],
        )?;
        let request_id = RequestId::new();
        let (sender, mut receiver) = event_channel(2);
        let mut sequence = Some(40);

        let complete = |through_sequence| {
            serde_json::to_string(&WebSocketServerMessage::EventsReplayComplete {
                request_id,
                desktop_id,
                desktop_generation,
                through_sequence,
            })
        };
        handle_server_text(
            &configuration,
            &sender,
            2,
            &complete(42)?,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("filtered replay boundary failed: {reason:?}"))?;
        assert_eq!(sequence, Some(42));
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        handle_server_text(
            &configuration,
            &sender,
            2,
            &complete(42)?,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("duplicate replay boundary failed: {reason:?}"))?;
        assert_eq!(sequence, Some(42));

        assert_eq!(
            handle_server_text(
                &configuration,
                &sender,
                2,
                &complete(41)?,
                request_id,
                &mut sequence,
            ),
            Err(EventStreamCloseReason::ResyncRequired)
        );
        assert_eq!(sequence, Some(42));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::SequenceRegression),
                dropped_through: None,
                latest_sequence: Some(42),
            }
        );
        Ok(())
    }

    #[test]
    fn invalid_subscription_messages_are_terminal_and_generation_aware() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let subscribed_topic = EventTopic::new("window.changed")?;
        let request_id = RequestId::new();
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            vec![subscribed_topic.clone()],
        )?;
        let message = |request_id, desktop_id, desktop_generation, topic| {
            serde_json::to_value(WebSocketServerMessage::Event {
                request_id,
                event: SequencedEvent {
                    desktop_id,
                    desktop_generation,
                    sequence: 11,
                    topic,
                    payload: serde_json::json!({"sequence": 11}),
                },
            })
        };
        let mut missing_request_id = message(
            request_id,
            desktop_id,
            desktop_generation,
            subscribed_topic.clone(),
        )?;
        missing_request_id
            .as_object_mut()
            .ok_or("event fixture was not an object")?
            .remove("request_id");
        let cases = [
            (missing_request_id, false),
            (
                message(
                    RequestId::new(),
                    desktop_id,
                    desktop_generation,
                    subscribed_topic.clone(),
                )?,
                false,
            ),
            (
                message(
                    request_id,
                    desktop_id,
                    desktop_generation,
                    EventTopic::new("process.exited")?,
                )?,
                false,
            ),
            (
                message(
                    request_id,
                    DesktopId::new(),
                    desktop_generation,
                    subscribed_topic.clone(),
                )?,
                false,
            ),
            (
                message(
                    request_id,
                    desktop_id,
                    DesktopGeneration::new(),
                    subscribed_topic,
                )?,
                true,
            ),
        ];

        for (raw, generation_changed) in cases {
            let (sender, mut receiver) = event_channel(1);
            let mut sequence = Some(10);
            assert_eq!(
                handle_server_text(
                    &configuration,
                    &sender,
                    1,
                    &serde_json::to_string(&raw)?,
                    request_id,
                    &mut sequence,
                ),
                Err(EventStreamCloseReason::InvalidMessage { generation_changed })
            );
            assert_eq!(sequence, Some(10));
            assert!(matches!(
                receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
        Ok(())
    }

    #[test]
    fn invalid_replay_and_resync_messages_are_terminal_and_generation_aware()
    -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let request_id = RequestId::new();
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            Vec::new(),
        )?;
        let replay = |request_id, desktop_id, desktop_generation| {
            serde_json::to_value(WebSocketServerMessage::EventsReplayComplete {
                request_id,
                desktop_id,
                desktop_generation,
                through_sequence: 12,
            })
        };
        let resync = |request_id, desktop_id, desktop_generation| {
            serde_json::to_value(WebSocketServerMessage::EventsResyncRequired {
                request_id,
                desktop_id,
                desktop_generation,
                reason: EventResyncReason::HistoryLost,
                dropped_through: 11,
                latest_sequence: 12,
            })
        };
        let mut cases = Vec::new();
        for mut missing in [
            replay(request_id, desktop_id, desktop_generation)?,
            resync(request_id, desktop_id, desktop_generation)?,
        ] {
            missing
                .as_object_mut()
                .ok_or("subscription fixture was not an object")?
                .remove("request_id");
            cases.push((missing, false));
        }
        for message in [
            replay(RequestId::new(), desktop_id, desktop_generation)?,
            resync(RequestId::new(), desktop_id, desktop_generation)?,
            replay(request_id, DesktopId::new(), desktop_generation)?,
            resync(request_id, DesktopId::new(), desktop_generation)?,
        ] {
            cases.push((message, false));
        }
        for message in [
            replay(request_id, desktop_id, DesktopGeneration::new())?,
            resync(request_id, desktop_id, DesktopGeneration::new())?,
        ] {
            cases.push((message, true));
        }

        for (raw, generation_changed) in cases {
            let (sender, mut receiver) = event_channel(1);
            let mut sequence = Some(10);
            assert_eq!(
                handle_server_text(
                    &configuration,
                    &sender,
                    1,
                    &serde_json::to_string(&raw)?,
                    request_id,
                    &mut sequence,
                ),
                Err(EventStreamCloseReason::InvalidMessage { generation_changed })
            );
            assert_eq!(sequence, Some(10));
            assert!(matches!(
                receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
        Ok(())
    }

    #[test]
    fn empty_replay_at_sequence_zero_sets_a_resumable_cursor() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            Vec::new(),
        )?;
        let request_id = RequestId::new();
        let message = serde_json::to_string(&WebSocketServerMessage::EventsReplayComplete {
            request_id,
            desktop_id,
            desktop_generation,
            through_sequence: 0,
        })?;
        let (sender, mut receiver) = event_channel(1);
        let mut sequence = None;
        handle_server_text(
            &configuration,
            &sender,
            1,
            &message,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("empty replay boundary failed: {reason:?}"))?;
        assert_eq!(sequence, Some(0));
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn duplicate_event_is_ignored_but_true_regression_is_terminal() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let topic = EventTopic::new("window.changed")?;
        let configuration = configuration(
            "http://127.0.0.1:8080",
            desktop_id,
            desktop_generation,
            vec![topic.clone()],
        )?;
        let request_id = RequestId::new();
        let event = |sequence| {
            serde_json::to_string(&WebSocketServerMessage::Event {
                request_id,
                event: SequencedEvent {
                    desktop_id,
                    desktop_generation,
                    sequence,
                    topic: topic.clone(),
                    payload: serde_json::json!({"sequence": sequence}),
                },
            })
        };
        let (sender, mut receiver) = event_channel(2);
        let mut sequence = Some(42);

        handle_server_text(
            &configuration,
            &sender,
            2,
            &event(42)?,
            request_id,
            &mut sequence,
        )
        .map_err(|reason| format!("duplicate event failed: {reason:?}"))?;
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        assert_eq!(
            handle_server_text(
                &configuration,
                &sender,
                2,
                &event(41)?,
                request_id,
                &mut sequence,
            ),
            Err(EventStreamCloseReason::ResyncRequired)
        );
        assert_eq!(sequence, Some(42));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::SequenceRegression),
                dropped_through: None,
                latest_sequence: Some(42),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn connect_waits_for_correlated_subscription_ack() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let topic = EventTopic::new("window.changed")?;
        let expected_topic = topic.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut socket = accept_async(stream).await?;
            let Message::Text(hello) = socket.next().await.ok_or("missing hello")?? else {
                return Err("hello was not text".into());
            };
            let hello: ClientHello = serde_json::from_str(hello.as_ref())?;
            if hello.resume.is_some() {
                return Err("hello created an unfiltered resume watch".into());
            }
            socket
                .send(Message::Text(
                    serde_json::to_string(&welcome(
                        desktop_id,
                        desktop_generation,
                        crate::MAX_RESPONSE_BYTES as u32,
                    ))?
                    .into(),
                ))
                .await?;
            let Message::Text(subscribe) = socket.next().await.ok_or("missing subscribe")?? else {
                return Err("subscribe was not text".into());
            };
            let subscribe: WebSocketClientMessage = serde_json::from_str(subscribe.as_ref())?;
            let WebSocketClientMessage::EventsSubscribe {
                request_id,
                desktop_id: actual_id,
                desktop_generation: actual_generation,
                topics,
                since_sequence,
            } = subscribe
            else {
                return Err("wrong subscribe message".into());
            };
            if actual_id != desktop_id
                || actual_generation != desktop_generation
                || topics != vec![expected_topic.clone()]
                || since_sequence != Some(17)
            {
                return Err("filtered replay subscription was incorrect".into());
            }
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::EventsSubscribed {
                        request_id,
                        topics: vec![expected_topic],
                    })?
                    .into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, desktop_generation, vec![topic])?;
        let result = timeout(
            Duration::from_secs(3),
            connect(&configuration, Some(17), false),
        )
        .await;
        assert!(result?.is_ok());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn stalled_hello_and_subscribe_sends_obey_absolute_deadline_and_cancellation()
    -> Result<(), TestError> {
        for message in [
            serde_json::json!({"type": "client.hello"}),
            serde_json::json!({"type": "events.subscribe"}),
        ] {
            let cancellation = CancellationToken::new();
            assert!(matches!(
                send_json_until(
                    &mut StalledPeerSink,
                    &message,
                    &cancellation,
                    Instant::now() + Duration::from_millis(10),
                )
                .await,
                Err(EventConnectError::Reconnectable)
            ));

            let cancellation = CancellationToken::new();
            cancellation.cancel();
            assert!(matches!(
                timeout(
                    Duration::from_millis(50),
                    send_json_until(
                        &mut StalledPeerSink,
                        &message,
                        &cancellation,
                        Instant::now() + Duration::from_secs(1),
                    ),
                )
                .await?,
                Err(EventConnectError::Reconnectable)
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn established_heartbeat_and_pong_writes_are_bounded_cancellable_and_drop_the_socket()
    -> Result<(), TestError> {
        let heartbeat = WebSocketClientMessage::Ping {
            request_id: RequestId::new(),
            nonce: RequestId::new().to_string(),
        };
        let messages = [
            Message::Text(serde_json::to_string(&heartbeat)?.into()),
            Message::Pong(vec![1, 2, 3].into()),
        ];
        for (index, message) in messages.into_iter().enumerate() {
            let dropped = Arc::new(AtomicUsize::new(0));
            let mut sink = ObservedStalledPeerSink {
                polled: Arc::new(tokio::sync::Notify::new()),
                dropped: dropped.clone(),
            };
            assert!(matches!(
                send_established_message(
                    &mut sink,
                    message.clone(),
                    &CancellationToken::new(),
                    Duration::from_millis(10),
                )
                .await,
                Err(EventConnectError::Reconnectable)
            ));
            drop(sink);
            assert_eq!(dropped.load(Ordering::SeqCst), 1, "timeout {index}");

            let cancellation = CancellationToken::new();
            let task_cancellation = cancellation.clone();
            let polled = Arc::new(tokio::sync::Notify::new());
            let dropped = Arc::new(AtomicUsize::new(0));
            let task_polled = polled.clone();
            let task_dropped = dropped.clone();
            let task = tokio::spawn(async move {
                let mut sink = ObservedStalledPeerSink {
                    polled: task_polled,
                    dropped: task_dropped,
                };
                send_established_message(
                    &mut sink,
                    message,
                    &task_cancellation,
                    Duration::from_secs(1),
                )
                .await
            });
            timeout(Duration::from_millis(100), polled.notified()).await?;
            cancellation.cancel();
            assert!(matches!(
                timeout(Duration::from_millis(100), task).await??,
                Err(EventConnectError::Reconnectable)
            ));
            assert_eq!(dropped.load(Ordering::SeqCst), 1, "cancellation {index}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn pre_welcome_close_classification_and_retry_counts_match_the_exact_table()
    -> Result<(), TestError> {
        for (close_code, expected) in [
            (None, "reconnectable"),
            (Some(1001_u16), "reconnectable"),
            (Some(1012), "reconnectable"),
            (Some(1013), "reconnectable"),
            (Some(1000), "protocol"),
            (Some(1002), "protocol"),
            (Some(1003), "protocol"),
            (Some(1007), "protocol"),
            (Some(1009), "protocol"),
            (Some(1011), "protocol"),
            (Some(4555), "protocol"),
            (Some(4401), "authentication"),
            (Some(4403), "permission"),
            (Some(1008), "permission"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let initial_server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut socket = accept_async(stream).await?;
                let _hello = socket.next().await.ok_or("missing hello")??;
                socket
                    .send(Message::Close(close_code.map(|code| CloseFrame {
                        code: CloseCode::from(code),
                        reason: "policy".into(),
                    })))
                    .await?;
                drop(socket);
                Ok::<bool, TestError>(
                    timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_ok(),
                )
            });
            let configuration = configuration(base, desktop_id, generation, Vec::new())?;
            let initial =
                timeout(Duration::from_secs(2), connect(&configuration, None, false)).await?;
            assert!(
                match expected {
                    "reconnectable" => matches!(initial, Err(EventConnectError::Reconnectable)),
                    "protocol" => matches!(initial, Err(EventConnectError::Protocol)),
                    "authentication" => matches!(initial, Err(EventConnectError::Authentication)),
                    "permission" => matches!(initial, Err(EventConnectError::Permission)),
                    _ => false,
                },
                "initial close {close_code:?} did not classify as {expected}"
            );
            assert!(
                !initial_server.await??,
                "initial close {close_code:?} opened an unexpected replacement"
            );

            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let reconnect_server = tokio::spawn(async move {
                let (mut initial, _, _, _) =
                    accept_subscription(&listener, desktop_id, generation, 1_000).await?;
                initial
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Restart,
                        reason: "restart".into(),
                    })))
                    .await?;
                drop(initial);

                let (stream, _) = listener.accept().await?;
                let mut reconnect = accept_async(stream).await?;
                let _hello = reconnect.next().await.ok_or("missing reconnect hello")??;
                reconnect
                    .send(Message::Close(close_code.map(|code| CloseFrame {
                        code: CloseCode::from(code),
                        reason: "policy".into(),
                    })))
                    .await?;
                drop(reconnect);
                let mut connection_count = 2_usize;
                if expected == "reconnectable" {
                    let (stream, _) = listener.accept().await?;
                    connection_count += 1;
                    let mut replacement = accept_async(stream).await?;
                    let _hello = replacement
                        .next()
                        .await
                        .ok_or("missing replacement hello")??;
                    replacement
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "terminal".into(),
                        })))
                        .await?;
                    drop(replacement);
                }
                Ok::<(usize, bool), TestError>((
                    connection_count,
                    timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_ok(),
                ))
            });
            let client = Client::from_options(
                crate::ClientOptions::builder(&base, "event-test-token-0123456789abcdef")
                    .reconnect_policy(crate::ReconnectPolicy::new(
                        3,
                        Duration::from_millis(1),
                        Duration::from_millis(1),
                    )?)
                    .build()?,
            )?;
            let configuration = EventConfiguration {
                cancellation: client.cancellation_token(),
                client,
                desktop_id,
                desktop_generation: generation,
                protocol: ProtocolVersion::V1_0,
                topics: Arc::new(Vec::new()),
            };
            let (socket, heartbeat, request_id) = connect(&configuration, None, false)
                .await
                .map_err(|error| format!("initial connect failed: {error:?}"))?;
            let (sender, mut receiver) = event_channel(2);
            timeout(
                Duration::from_secs(2),
                supervise(
                    configuration,
                    sender,
                    2,
                    socket,
                    heartbeat,
                    request_id,
                    None,
                ),
            )
            .await?;
            match expected {
                "authentication" | "permission" => {
                    let expected_code = if expected == "authentication" {
                        ErrorCode::AuthenticationRequired
                    } else {
                        ErrorCode::PermissionDenied
                    };
                    assert_eq!(
                        receiver.try_recv()?,
                        EventStreamItem::ServerError {
                            request_id: None,
                            code: expected_code,
                            detail: if expected == "authentication" {
                                "event authentication failed".to_owned()
                            } else {
                                "event permission denied".to_owned()
                            },
                        }
                    );
                    assert_eq!(
                        receiver.try_recv()?,
                        EventStreamItem::Closed {
                            reason: EventStreamCloseReason::ServerError(expected_code)
                        }
                    );
                }
                "reconnectable" | "protocol" => {
                    assert_eq!(
                        receiver.try_recv()?,
                        EventStreamItem::Closed {
                            reason: EventStreamCloseReason::ProtocolViolation
                        },
                        "reconnect close {close_code:?}"
                    );
                }
                _ => return Err("unknown close classification".into()),
            }
            let (connection_count, unexpected_replacement) = reconnect_server.await??;
            assert!(
                !unexpected_replacement,
                "reconnect close {close_code:?} opened an unexpected extra replacement"
            );
            assert_eq!(
                connection_count,
                if expected == "reconnectable" { 3 } else { 2 },
                "reconnect close {close_code:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn mismatched_ack_and_excessive_advertised_frame_limit_fail_connect()
    -> Result<(), TestError> {
        for excessive_limit in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut socket = accept_async(stream).await?;
                let _hello = socket.next().await.ok_or("missing hello")??;
                let limit = if excessive_limit {
                    crate::MAX_RESPONSE_BYTES as u32 + 1
                } else {
                    crate::MAX_RESPONSE_BYTES as u32
                };
                socket
                    .send(Message::Text(
                        serde_json::to_string(&welcome(desktop_id, generation, limit))?.into(),
                    ))
                    .await?;
                if !excessive_limit {
                    let Message::Text(subscribe) =
                        socket.next().await.ok_or("missing subscribe")??
                    else {
                        return Err("subscribe was not text".into());
                    };
                    let WebSocketClientMessage::EventsSubscribe { topics, .. } =
                        serde_json::from_str(subscribe.as_ref())?
                    else {
                        return Err("wrong subscribe message".into());
                    };
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&WebSocketServerMessage::EventsSubscribed {
                                request_id: RequestId::new(),
                                topics,
                            })?
                            .into(),
                        ))
                        .await?;
                }
                Ok::<(), TestError>(())
            });
            let configuration = configuration(base, desktop_id, generation, Vec::new())?;
            assert!(matches!(
                timeout(Duration::from_secs(3), connect(&configuration, None, false),).await?,
                Err(EventConnectError::Protocol)
            ));
            server.await??;
        }
        Ok(())
    }

    #[tokio::test]
    async fn http_auth_rejection_is_permanent_and_never_retried() -> Result<(), TestError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 4096];
            let _read = stream.read(&mut request).await?;
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            Ok::<(), std::io::Error>(())
        });
        let configuration =
            configuration(base, DesktopId::new(), DesktopGeneration::new(), Vec::new())?;
        assert!(matches!(
            timeout(Duration::from_secs(3), connect(&configuration, None, false),).await?,
            Err(EventConnectError::Authentication)
        ));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn heartbeat_requires_the_correlated_random_nonce() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut socket = accept_async(stream).await?;
            let _hello = socket.next().await.ok_or("missing hello")??;
            let mut welcome = welcome(desktop_id, generation, crate::MAX_RESPONSE_BYTES as u32);
            if let WebSocketServerMessage::Welcome { limits, .. } = &mut welcome {
                limits.heartbeat_ms = 300;
            }
            socket
                .send(Message::Text(serde_json::to_string(&welcome)?.into()))
                .await?;
            let Message::Text(subscribe) = socket.next().await.ok_or("missing subscribe")?? else {
                return Err("subscribe was not text".into());
            };
            let WebSocketClientMessage::EventsSubscribe {
                request_id, topics, ..
            } = serde_json::from_str(subscribe.as_ref())?
            else {
                return Err("wrong subscribe".into());
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::EventsSubscribed {
                        request_id,
                        topics,
                    })?
                    .into(),
                ))
                .await?;
            let Message::Text(ping) = socket.next().await.ok_or("missing ping")?? else {
                return Err("ping was not text".into());
            };
            let WebSocketClientMessage::Ping { request_id, .. } =
                serde_json::from_str(ping.as_ref())?
            else {
                return Err("wrong ping".into());
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::Pong {
                        request_id,
                        nonce: "not-the-random-client-nonce".to_owned(),
                    })?
                    .into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (mut socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, _receiver) = event_channel(2);
        let outcome = timeout(
            Duration::from_secs(3),
            run_socket(
                &configuration,
                &sender,
                2,
                &mut socket,
                heartbeat,
                request_id,
                &mut None,
            ),
        )
        .await?;
        assert_eq!(
            outcome,
            SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation)
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_reconnects_and_resumes_from_the_last_delivered_sequence()
    -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let topic = EventTopic::new("window.changed")?;
        let server_topic = topic.clone();
        let server = tokio::spawn(async move {
            let (mut first, request_id, topics, since_sequence) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            if topics != vec![server_topic.clone()] || since_sequence.is_some() {
                return Err("initial subscription cursor was incorrect".into());
            }
            send_event(
                &mut first,
                request_id,
                desktop_id,
                generation,
                1,
                server_topic.clone(),
            )
            .await?;
            first
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Restart,
                    reason: "restart".into(),
                })))
                .await?;
            match timeout(Duration::from_millis(250), first.next()).await {
                Ok(None | Some(Ok(Message::Close(_)))) => {}
                Ok(Some(Ok(other))) => {
                    return Err(format!("old socket yielded unexpected message: {other:?}").into());
                }
                Ok(Some(Err(_))) => {}
                Err(_) => return Err("old socket was retained into reconnect backoff".into()),
            }

            let (mut second, request_id, topics, since_sequence) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            if topics != vec![server_topic.clone()] || since_sequence != Some(1) {
                return Err("reconnect did not resume from sequence one".into());
            }
            send_event(
                &mut second,
                request_id,
                desktop_id,
                generation,
                2,
                server_topic,
            )
            .await?;
            while second.next().await.is_some() {}
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, vec![topic])?;
        let (socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(4);
        let supervisor_configuration = configuration.clone();
        let task = tokio::spawn(async move {
            supervise(
                supervisor_configuration,
                sender,
                4,
                socket,
                heartbeat,
                request_id,
                None,
            )
            .await;
        });
        for expected in [1, 2] {
            let item = timeout(Duration::from_secs(3), receiver.recv())
                .await?
                .ok_or("stream ended before resumed event")?;
            let EventStreamItem::Event(event) = item else {
                return Err(format!("unexpected resumed item: {item:?}").into());
            };
            if event.sequence != expected {
                return Err(format!("expected sequence {expected}, got {}", event.sequence).into());
            }
        }
        configuration.cancellation.cancel();
        assert_eq!(
            timeout(Duration::from_secs(2), receiver.recv()).await?,
            Some(EventStreamItem::Closed {
                reason: EventStreamCloseReason::ClientClosed
            })
        );
        task.await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_stops_after_bounded_reconnect_exhaustion() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let topic = EventTopic::new("window.changed")?;
        let server = tokio::spawn(async move {
            let (mut initial, _, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            initial
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Restart,
                    reason: "restart".into(),
                })))
                .await?;
            let (failed_reconnect, _) = listener.accept().await?;
            drop(failed_reconnect);
            Ok::<(), TestError>(())
        });
        let client = Client::from_options(
            crate::ClientOptions::builder(&base, "event-test-token-0123456789abcdef")
                .reconnect_policy(crate::ReconnectPolicy::new(
                    1,
                    Duration::from_millis(1),
                    Duration::from_millis(1),
                )?)
                .build()?,
        )?;
        let configuration = EventConfiguration {
            cancellation: client.cancellation_token(),
            client,
            desktop_id,
            desktop_generation: generation,
            protocol: ProtocolVersion::V1_0,
            topics: Arc::new(vec![topic]),
        };
        let (socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(2);
        timeout(
            Duration::from_secs(2),
            supervise(
                configuration,
                sender,
                2,
                socket,
                heartbeat,
                request_id,
                None,
            ),
        )
        .await?;
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::ReconnectExhausted
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn structured_application_error_is_delivered_before_terminal_reason()
    -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, request_id, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::Error {
                        request_id: Some(request_id),
                        code: ErrorCode::InvalidRequest,
                        detail: "subscription rejected".to_owned(),
                        desktop_generation: Some(generation),
                    })?
                    .into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(2);
        supervise(
            configuration,
            sender,
            2,
            socket,
            heartbeat,
            request_id,
            None,
        )
        .await;
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::ServerError {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::ServerError(ErrorCode::InvalidRequest)
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn permanent_peer_close_code_is_terminal_and_not_reconnected() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, _, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            socket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "permanent".into(),
                })))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(1);
        supervise(
            configuration,
            sender,
            1,
            socket,
            heartbeat,
            request_id,
            None,
        )
        .await;
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::PeerClosed(1008)
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn missing_application_pong_or_any_read_forces_reconnect() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, _, _, _) =
                accept_subscription(&listener, desktop_id, generation, 300).await?;
            let Message::Text(ping) = socket.next().await.ok_or("missing heartbeat ping")?? else {
                return Err("heartbeat ping was not text".into());
            };
            if !matches!(
                serde_json::from_str::<WebSocketClientMessage>(ping.as_ref())?,
                WebSocketClientMessage::Ping { .. }
            ) {
                return Err("heartbeat message was not client.ping".into());
            }
            while socket.next().await.is_some() {}
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (mut socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, _receiver) = event_channel(1);
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                run_socket(
                    &configuration,
                    &sender,
                    1,
                    &mut socket,
                    heartbeat,
                    request_id,
                    &mut None,
                ),
            )
            .await?,
            SessionEnd::Reconnect
        );
        drop(socket);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn oversized_real_websocket_text_frame_is_a_protocol_violation() -> Result<(), TestError>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, _, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            socket
                .send(Message::Text(
                    "x".repeat(crate::MAX_RESPONSE_BYTES + 1).into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (mut socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, _receiver) = event_channel(1);
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                run_socket(
                    &configuration,
                    &sender,
                    1,
                    &mut socket,
                    heartbeat,
                    request_id,
                    &mut None,
                ),
            )
            .await?,
            SessionEnd::Terminal(EventStreamCloseReason::ProtocolViolation)
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn real_socket_overflow_preserves_explicit_terminal_reason() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, _, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            socket
                .send(Message::Text(
                    r#"{"type":"future.notice","ordinal":1}"#.into(),
                ))
                .await?;
            socket
                .send(Message::Text(
                    r#"{"type":"future.notice","ordinal":2}"#.into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (socket, heartbeat, request_id) = connect(&configuration, None, false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(1);
        supervise(
            configuration,
            sender,
            1,
            socket,
            heartbeat,
            request_id,
            None,
        )
        .await;
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::UnknownMessage { .. }
        ));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::QueueOverflow
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn real_socket_resync_observation_is_followed_by_terminal_reason() -> Result<(), TestError>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let server = tokio::spawn(async move {
            let (mut socket, request_id, _, _) =
                accept_subscription(&listener, desktop_id, generation, 1_000).await?;
            let topic = EventTopic::new("window.changed")?;
            send_event(
                &mut socket,
                request_id,
                desktop_id,
                generation,
                8,
                topic.clone(),
            )
            .await?;
            send_event(&mut socket, request_id, desktop_id, generation, 9, topic).await?;
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::EventsResyncRequired {
                        request_id,
                        desktop_id,
                        desktop_generation: generation,
                        reason: EventResyncReason::HistoryLost,
                        dropped_through: 40,
                        latest_sequence: 42,
                    })?
                    .into(),
                ))
                .await?;
            Ok::<(), TestError>(())
        });
        let configuration = configuration(base, desktop_id, generation, Vec::new())?;
        let (socket, heartbeat, request_id) = connect(&configuration, Some(7), false)
            .await
            .map_err(|error| format!("connect failed: {error:?}"))?;
        let (sender, mut receiver) = event_channel(2);
        supervise(
            configuration,
            sender,
            2,
            socket,
            heartbeat,
            request_id,
            Some(7),
        )
        .await;
        for expected_sequence in [8, 9] {
            assert!(matches!(
                receiver.try_recv()?,
                EventStreamItem::Event(SequencedEvent { sequence, .. })
                    if sequence == expected_sequence
            ));
        }
        assert!(matches!(
            receiver.try_recv()?,
            EventStreamItem::ResyncRequired {
                reason: Some(EventStreamResyncReason::HistoryLost),
                latest_sequence: Some(42),
                ..
            }
        ));
        assert_eq!(
            receiver.try_recv()?,
            EventStreamItem::Closed {
                reason: EventStreamCloseReason::ResyncRequired
            }
        );
        server.await??;
        Ok(())
    }

    #[test]
    fn reconnect_backoff_is_exponential_jittered_and_capped() -> Result<(), TestError> {
        let policy =
            crate::ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_millis(450))?;
        assert_eq!(reconnect_delay(policy, 1, 0), Duration::from_millis(100));
        assert_eq!(reconnect_delay(policy, 2, 0), Duration::from_millis(200));
        assert_eq!(reconnect_delay(policy, 3, 0), Duration::from_millis(400));
        assert_eq!(reconnect_delay(policy, 4, 0), Duration::from_millis(450));
        assert!(reconnect_delay(policy, 1, u8::MAX) > Duration::from_millis(100));
        assert!(reconnect_delay(policy, 1, u8::MAX) <= Duration::from_millis(125));
        assert_eq!(
            reconnect_delay(policy, 4, u8::MAX),
            Duration::from_millis(450)
        );
        Ok(())
    }
}
