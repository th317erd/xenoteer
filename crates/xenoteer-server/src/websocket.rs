//! Version-one WebSocket handshake, control messages, and priority output.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    time::Duration,
};

use axum::{
    Extension,
    extract::{
        State,
        ws::{
            CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade,
            rejection::WebSocketUpgradeRejection,
        },
    },
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc, watch},
    task::{AbortHandle, JoinHandle, JoinSet},
};
use xenoteer_protocol::{
    ClientHello, CommandEnvelope, CommandId, CommandResult, ConnectionId, DesktopGeneration,
    DesktopId, ErrorCode, EventResumeStatus, EventResyncReason, EventTopic, LeaseAcquireRequest,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, MAX_EVENT_TOPICS, ProtocolVersion,
    RequestId, SequencedEvent, ViewerOrigin, WebSocketClientMessage as ClientMessage,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{
        CommandCancellation, CommandSubmission, CommandWait, ControlPlaneError,
        ControlRequestContext, EventReplay, EventSubscription, LiveEvent, SubmissionDisposition,
    },
    problem::ApiProblem,
};

const WATCH_WAIT: Duration = Duration::from_secs(25);
const FAST_WATCH_RETRY_DELAY: Duration = Duration::from_millis(100);
const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CLOSE_DRAIN_MESSAGES: usize = 256;
const MAX_SESSION_COMMAND_WATCHES: usize = 256;

/// Exact browser-Origin allowlist. SDK requests may omit Origin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedOrigins {
    exact: BTreeSet<String>,
}

impl AllowedOrigins {
    /// Validates and stores exact `http`/`https` origins.
    pub fn exact(origins: impl IntoIterator<Item = String>) -> Result<Self, OriginPolicyError> {
        let mut exact = BTreeSet::new();
        for origin in origins {
            let origin = ViewerOrigin::new(origin).map_err(|_| OriginPolicyError::InvalidOrigin)?;
            exact.insert(origin.as_str().to_owned());
        }
        Ok(Self { exact })
    }

    pub(crate) fn permits_origin(&self, origin: &ViewerOrigin) -> bool {
        self.exact.contains(origin.as_str())
    }

    fn permits(&self, headers: &HeaderMap) -> bool {
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            return true;
        };
        if origins.next().is_some() {
            return false;
        }
        origin
            .to_str()
            .ok()
            .is_some_and(|origin| self.exact.contains(origin))
    }
}

/// Invalid browser-Origin configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OriginPolicyError {
    /// Origins must use the protocol's bounded canonical HTTP(S) representation.
    #[error("WebSocket origin allowlist entry is invalid")]
    InvalidOrigin,
}

#[derive(Serialize)]
struct ServerWelcome {
    #[serde(rename = "type")]
    message_type: &'static str,
    protocol: ProtocolVersion,
    connection_id: ConnectionId,
    principal: WelcomePrincipal,
    desktop: WelcomeDesktop,
    limits: WelcomeLimits,
    resume: WelcomeResume,
}

#[derive(Serialize)]
struct WelcomePrincipal {
    id: String,
    capabilities: Vec<String>,
}

#[derive(Serialize)]
struct WelcomeDesktop {
    id: DesktopId,
    generation: Option<DesktopGeneration>,
    state: crate::DesktopReadiness,
}

#[derive(Serialize)]
struct WelcomeLimits {
    max_message_bytes: u32,
    heartbeat_ms: u32,
    normal_outbound_capacity: u32,
    reserved_outbound_capacity: u32,
    max_command_watches: u32,
}

#[derive(Serialize)]
struct WelcomeResume {
    status: EventResumeStatus,
}

#[derive(Deserialize)]
struct MessageHeader {
    #[serde(rename = "type")]
    message_type: String,
    request_id: Option<RequestId>,
}

#[derive(Serialize)]
struct ServerPong {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    nonce: String,
}

#[derive(Serialize)]
struct ServerError {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: Option<RequestId>,
    code: ErrorCode,
    detail: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_generation: Option<DesktopGeneration>,
}

#[derive(Serialize)]
struct ServerCommand<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    result: &'a CommandResult,
}

#[derive(Serialize)]
struct ServerLease<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    lease: &'a LeaseStateView,
}

#[derive(Serialize)]
struct ServerWatchState {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    command_id: CommandId,
    watching: bool,
}

#[derive(Serialize)]
struct ServerEventSubscription<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    topics: &'a [EventTopic],
}

#[derive(Serialize)]
struct ServerEventUnsubscribed {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
}

#[derive(Serialize)]
struct ServerEvent<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    event: &'a SequencedEvent,
}

#[derive(Serialize)]
struct ServerReplayComplete {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    #[serde(serialize_with = "serialize_u64_string")]
    through_sequence: u64,
}

#[derive(Serialize)]
struct ServerResyncRequired {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    reason: EventResyncReason,
    #[serde(serialize_with = "serialize_u64_string")]
    dropped_through: u64,
    #[serde(serialize_with = "serialize_u64_string")]
    latest_sequence: u64,
}

#[derive(Serialize)]
struct ServerDraining<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    desktop_id: DesktopId,
    desktop_generation: Option<DesktopGeneration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct WireFailure {
    code: ErrorCode,
    detail: &'static str,
    desktop_generation: Option<DesktopGeneration>,
}

impl WireFailure {
    const fn new(code: ErrorCode, detail: &'static str) -> Self {
        Self {
            code,
            detail,
            desktop_generation: None,
        }
    }

    const fn stale(current: Option<DesktopGeneration>) -> Self {
        Self {
            code: ErrorCode::StaleReference,
            detail: "The request targets an earlier desktop generation.",
            desktop_generation: current,
        }
    }
}

#[derive(Clone)]
struct OutboundQueues {
    normal: mpsc::Sender<Message>,
    high: mpsc::Sender<Message>,
}

struct OutboundReceiver {
    normal: mpsc::Receiver<Message>,
    high: mpsc::Receiver<Message>,
    normal_open: bool,
    high_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DroppableSend {
    Enqueued,
    Dropped,
}

impl OutboundQueues {
    fn bounded(normal_capacity: usize, high_capacity: usize) -> (Self, OutboundReceiver) {
        let (normal, normal_rx) = mpsc::channel(normal_capacity);
        let (high, high_rx) = mpsc::channel(high_capacity);
        (
            Self { normal, high },
            OutboundReceiver {
                normal: normal_rx,
                high: high_rx,
                normal_open: true,
                high_open: true,
            },
        )
    }

    async fn normal_json<T: Serialize>(&self, value: &T) -> Result<(), ()> {
        send_serialized(&self.normal, value).await
    }

    fn progress_json<T: Serialize>(&self, value: &T) -> Result<(), ()> {
        self.droppable_json(value).map(|_| ())
    }

    fn droppable_json<T: Serialize>(&self, value: &T) -> Result<DroppableSend, ()> {
        let message = encode_message(value)?;
        match self.normal.try_send(message) {
            Ok(()) => Ok(DroppableSend::Enqueued),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(DroppableSend::Dropped),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }

    async fn high_json<T: Serialize>(&self, value: &T) -> Result<(), ()> {
        send_serialized(&self.high, value).await
    }

    async fn high_message(&self, message: Message) -> Result<(), ()> {
        self.high.send(message).await.map_err(|_| ())
    }
}

impl OutboundReceiver {
    async fn recv(&mut self) -> Option<Message> {
        loop {
            if !self.normal_open && !self.high_open {
                return None;
            }
            tokio::select! {
                biased;
                message = self.high.recv(), if self.high_open => {
                    match message {
                        Some(message) => return Some(message),
                        None => self.high_open = false,
                    }
                }
                message = self.normal.recv(), if self.normal_open => {
                    match message {
                        Some(message) => return Some(message),
                        None => self.normal_open = false,
                    }
                }
            }
        }
    }
}

struct OwnedWatch {
    serial: u64,
    abort: AbortHandle,
}

struct WatchSet {
    tasks: JoinSet<(CommandId, u64)>,
    owned: BTreeMap<CommandId, OwnedWatch>,
    next_serial: u64,
}

impl WatchSet {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            owned: BTreeMap::new(),
            next_serial: 1,
        }
    }

    fn start(
        &mut self,
        state: ApiState,
        principal: Principal,
        request_id: RequestId,
        desktop_id: DesktopId,
        command_id: CommandId,
        outbound: OutboundQueues,
    ) -> bool {
        self.spawn(command_id, async move {
            watch_command(
                state, principal, request_id, desktop_id, command_id, outbound,
            )
            .await;
        })
    }

    fn spawn<F>(&mut self, command_id: CommandId, task: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if !self.owned.contains_key(&command_id) && self.owned.len() >= MAX_SESSION_COMMAND_WATCHES
        {
            return false;
        }
        self.stop(command_id);
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        let abort = self.tasks.spawn(async move {
            task.await;
            (command_id, serial)
        });
        self.owned.insert(command_id, OwnedWatch { serial, abort });
        true
    }

    fn stop(&mut self, command_id: CommandId) -> bool {
        if let Some(watch) = self.owned.remove(&command_id) {
            watch.abort.abort();
            true
        } else {
            false
        }
    }

    fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    async fn reap_one(&mut self) {
        match self.tasks.join_next().await {
            Some(Ok((command_id, serial))) => {
                if self
                    .owned
                    .get(&command_id)
                    .is_some_and(|watch| watch.serial == serial)
                {
                    self.owned.remove(&command_id);
                }
            }
            Some(Err(_)) => {
                self.owned.retain(|_, watch| !watch.abort.is_finished());
            }
            None => {}
        }
    }

    async fn shutdown(&mut self) {
        for watch in self.owned.values() {
            watch.abort.abort();
        }
        self.owned.clear();
        while self.tasks.join_next().await.is_some() {}
    }
}

struct EventWatch {
    task: Option<JoinHandle<()>>,
}

struct EventDeliveryTarget {
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    topics: Vec<EventTopic>,
    allow_accessibility: bool,
}

impl EventWatch {
    const fn new() -> Self {
        Self { task: None }
    }

    fn start(
        &mut self,
        subscription: EventSubscription,
        target: EventDeliveryTarget,
        outbound: OutboundQueues,
    ) {
        self.stop();
        self.task = Some(tokio::spawn(deliver_events(subscription, target, outbound)));
    }

    fn stop(&mut self) -> bool {
        if let Some(task) = self.task.take() {
            task.abort();
            true
        } else {
            false
        }
    }
}

impl Drop for EventWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) async fn upgrade(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !state.origins.permits(&headers) {
        return ApiProblem::origin_denied(request_id).into_response();
    }
    let Some(session_permit) = state.abuse.try_acquire_websocket() else {
        return ApiProblem::resource_exhausted(request_id).into_response();
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(_) => return ApiProblem::invalid_request(request_id).into_response(),
    };
    let limits = state.limits;
    upgrade
        .max_message_size(limits.max_body_bytes())
        .max_frame_size(limits.max_body_bytes())
        .write_buffer_size(0)
        .max_write_buffer_size(limits.max_body_bytes().saturating_add(64 * 1_024))
        .on_failed_upgrade(|_| tracing::debug!("WebSocket upgrade failed"))
        .on_upgrade(move |socket| session(socket, state, principal, session_permit))
}

async fn session(
    socket: WebSocket,
    state: ApiState,
    principal: Principal,
    session_permit: OwnedSemaphorePermit,
) {
    let (sender, receiver) = socket.split();
    let (outbound, outbound_rx) = OutboundQueues::bounded(
        state.limits.ws_outbound_capacity(),
        state.limits.ws_high_priority_capacity(),
    );
    let (writer_stopped_tx, writer_stopped_rx) = watch::channel(false);
    let write_timeout = state.limits.request_timeout();
    let reader = read_session(receiver, outbound, writer_stopped_rx, state, principal);
    let writer = write_session(sender, outbound_rx, writer_stopped_tx, write_timeout);
    let (_reader_result, _writer_result) = tokio::join!(reader, writer);
    drop(session_permit);
}

async fn read_session(
    mut receiver: futures_util::stream::SplitStream<WebSocket>,
    outbound: OutboundQueues,
    mut writer_stopped: watch::Receiver<bool>,
    state: ApiState,
    principal: Principal,
) {
    let hello = match tokio::time::timeout(state.limits.ws_hello_timeout(), receiver.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => parse_hello(&text),
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return,
        Ok(Some(Err(error))) => {
            if let Some(message) = websocket_read_error_close(&error) {
                let _ignored = outbound.high_message(message).await;
            }
            return;
        }
        _ => Err(HandshakeError::InvalidMessage),
    };
    let (hello, protocol) = match hello {
        Ok(hello) => hello,
        Err(error) => {
            let code = if error == HandshakeError::UnsupportedVersion {
                ErrorCode::UnsupportedVersion
            } else {
                ErrorCode::InvalidRequest
            };
            let _ignored =
                send_error(&outbound, None, WireFailure::new(code, error.safe_detail())).await;
            let _ignored = outbound
                .high_message(close(1002, "invalid handshake"))
                .await;
            return;
        }
    };

    let readiness = state.readiness.snapshot();
    let mut resumed = None;
    let resume_status = match &hello.resume {
        None => EventResumeStatus::NotRequested,
        Some(_) if !principal.has_grant(Grant::DesktopObserve) => {
            let _ignored = send_permission_denied(&outbound, hello.request_id).await;
            let _ignored = outbound.high_message(close(1008, "resume denied")).await;
            return;
        }
        Some(resume)
            if resume.desktop_id != state.desktop_id
                || readiness.desktop_generation != Some(resume.desktop_generation) =>
        {
            EventResumeStatus::ResyncRequired
        }
        Some(resume) => match state
            .control
            .subscribe_events(
                ControlRequestContext::new(principal.clone(), hello.request_id),
                resume.desktop_id,
                resume.desktop_generation,
                Some(resume.event_sequence),
            )
            .await
        {
            Ok(subscription) => {
                let status = if matches!(&subscription.replay, EventReplay::Events { .. }) {
                    EventResumeStatus::Replayed
                } else {
                    EventResumeStatus::ResyncRequired
                };
                resumed = Some((resume.clone(), subscription));
                status
            }
            Err(ControlPlaneError::StaleReference { .. }) => EventResumeStatus::ResyncRequired,
            Err(error) => {
                let _ignored = send_control_error(&outbound, hello.request_id, error).await;
                let _ignored = outbound.high_message(close(1011, "resume failed")).await;
                return;
            }
        },
    };
    // Welcome must be the first application message. Resume processing can
    // immediately enqueue a reserved resync notice, so putting welcome on the
    // normal lane would let the biased high-priority writer overtake it.
    let Some(limits) = welcome_limits(state.limits) else {
        let _ignored = outbound.high_message(close(1011, "server error")).await;
        return;
    };
    if outbound
        .high_json(&ServerWelcome {
            message_type: "server.welcome",
            protocol,
            connection_id: ConnectionId::new(),
            principal: WelcomePrincipal {
                id: principal.id().to_owned(),
                capabilities: principal.grant_names().map(str::to_owned).collect(),
            },
            desktop: WelcomeDesktop {
                id: state.desktop_id,
                generation: readiness.desktop_generation,
                state: readiness.state,
            },
            limits,
            resume: WelcomeResume {
                status: resume_status,
            },
        })
        .await
        .is_err()
    {
        return;
    }

    let stale = tokio::time::sleep(state.limits.ws_stale_timeout());
    tokio::pin!(stale);
    let mut watches = WatchSet::new();
    let mut event_watch = EventWatch::new();
    if let Some((resume, subscription)) = resumed {
        event_watch.start(
            subscription,
            EventDeliveryTarget {
                request_id: hello.request_id,
                desktop_id: resume.desktop_id,
                desktop_generation: resume.desktop_generation,
                topics: Vec::new(),
                allow_accessibility: principal.has_grant(Grant::AccessibilityRead),
            },
            outbound.clone(),
        );
    }
    let mut readiness_updates = state.readiness.subscribe();
    let mut draining = matches!(
        readiness.state,
        crate::DesktopReadiness::Draining
            | crate::DesktopReadiness::Stopped
            | crate::DesktopReadiness::Failed
    )
    .then_some(readiness);
    let mut message_limit = state.abuse.websocket_message_limit();
    while draining.is_none() {
        let has_watch_tasks = watches.has_tasks();
        tokio::select! {
            changed = writer_stopped.changed() => {
                if changed.is_err() || *writer_stopped.borrow() {
                    break;
                }
            }
            changed = readiness_updates.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = readiness_updates.borrow_and_update().clone();
                if matches!(
                    snapshot.state,
                    crate::DesktopReadiness::Draining
                        | crate::DesktopReadiness::Stopped
                        | crate::DesktopReadiness::Failed
                ) {
                    draining = Some(snapshot);
                    break;
                }
            }
            () = &mut stale => {
                let _ignored = outbound.high_message(close(1001, "heartbeat timeout")).await;
                break;
            }
            () = watches.reap_one(), if has_watch_tasks => {}
            message = receiver.next() => {
                let Some(message) = message else { break; };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        if let Some(message) = websocket_read_error_close(&error) {
                            let _ignored = outbound.high_message(message).await;
                        }
                        break;
                    }
                };
                match message {
                    Message::Text(text) => {
                        stale.as_mut().reset(
                            tokio::time::Instant::now() + state.limits.ws_stale_timeout()
                        );
                        if !message_limit.try_take() {
                            let _ignored = send_message_rate_exhausted(&outbound, &text).await;
                            drain_after_server_close(&mut receiver).await;
                            break;
                        }
                        if handle_text(
                            &outbound,
                            &state,
                            &principal,
                            &mut watches,
                            &mut event_watch,
                            protocol,
                            &text,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        let _ignored = outbound
                            .high_message(close(1003, "text messages required"))
                            .await;
                        break;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
        }
    }
    event_watch.stop();
    if let Some(snapshot) = draining {
        drain_session(&outbound, &mut watches, &state, &snapshot).await;
    }
    watches.shutdown().await;
}

async fn drain_after_server_close(receiver: &mut futures_util::stream::SplitStream<WebSocket>) {
    let deadline = tokio::time::Instant::now() + CLOSE_DRAIN_TIMEOUT;
    for _ in 0..MAX_CLOSE_DRAIN_MESSAGES {
        match tokio::time::timeout_at(deadline, receiver.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => return,
            Ok(Some(Ok(_))) => {}
        }
    }
}

async fn drain_session(
    outbound: &OutboundQueues,
    watches: &mut WatchSet,
    state: &ApiState,
    snapshot: &crate::ReadinessSnapshot,
) {
    let _ignored = outbound
        .high_json(&ServerDraining {
            message_type: "server.draining",
            desktop_id: state.desktop_id,
            desktop_generation: snapshot.desktop_generation,
            reason_code: snapshot.reason_code.as_deref(),
        })
        .await;
    let deadline = tokio::time::sleep(state.limits.request_timeout());
    tokio::pin!(deadline);
    loop {
        if !watches.has_tasks() {
            break;
        }
        tokio::select! {
            () = &mut deadline => break,
            () = watches.reap_one() => {}
        }
    }
    let _ignored = outbound.high_message(close(1001, "server draining")).await;
}

async fn write_session(
    mut sender: futures_util::stream::SplitSink<WebSocket, Message>,
    mut outbound: OutboundReceiver,
    writer_stopped: watch::Sender<bool>,
    write_timeout: Duration,
) {
    while let Some(message) = outbound.recv().await {
        let closing = matches!(message, Message::Close(_));
        let sent = tokio::time::timeout(write_timeout, sender.send(message)).await;
        if !matches!(sent, Ok(Ok(()))) || closing {
            break;
        }
    }
    let _ignored = writer_stopped.send(true);
}

fn parse_hello(text: &str) -> Result<(ClientHello, ProtocolVersion), HandshakeError> {
    let hello: ClientHello =
        serde_json::from_str(text).map_err(|_| HandshakeError::InvalidMessage)?;
    if hello.validate().is_err() {
        return Err(HandshakeError::InvalidMessage);
    }
    let selected = crate::protocol_version::negotiate(hello.protocol)
        .map_err(|_| HandshakeError::UnsupportedVersion)?;
    Ok((hello, selected))
}

async fn handle_text(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    watches: &mut WatchSet,
    event_watch: &mut EventWatch,
    negotiated_protocol: ProtocolVersion,
    text: &str,
) -> Result<(), ()> {
    let header: MessageHeader = match serde_json::from_str(text) {
        Ok(header) => header,
        Err(_) => {
            return send_protocol_error(
                outbound,
                None,
                "The message is not valid version-one JSON.",
            )
            .await;
        }
    };
    if header.message_type.is_empty() || header.message_type.len() > 128 {
        return send_protocol_error(
            outbound,
            header.request_id,
            "The message type is missing or exceeds its protocol bound.",
        )
        .await;
    }
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(_) => {
            return send_protocol_error(
                outbound,
                header.request_id,
                "The message does not match a version-one protocol shape.",
            )
            .await;
        }
    };
    match message {
        ClientMessage::Ping { request_id, nonce } => {
            if request_id.as_uuid().is_nil()
                || nonce.is_empty()
                || nonce.len() > 128
                || nonce.chars().any(char::is_control)
            {
                return send_protocol_error(
                    outbound,
                    Some(request_id),
                    "The client.ping message contains an invalid identifier or nonce.",
                )
                .await;
            }
            outbound
                .normal_json(&ServerPong {
                    message_type: "server.pong",
                    request_id,
                    nonce,
                })
                .await
        }
        ClientMessage::CommandSubmit {
            request_id,
            command,
        } => {
            if command.protocol_version != negotiated_protocol {
                send_unsupported_message_version(outbound, request_id).await
            } else {
                handle_command_submit(outbound, state, principal, watches, request_id, *command)
                    .await
            }
        }
        ClientMessage::CommandWatch {
            request_id,
            desktop_id,
            desktop_generation,
            command_id,
        } => {
            handle_command_watch(
                outbound,
                state,
                principal,
                watches,
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            )
            .await
        }
        ClientMessage::CommandUnwatch {
            request_id,
            desktop_id,
            desktop_generation,
            command_id,
        } => {
            if let Err(error) = validate_observe_request(
                state,
                principal,
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            ) {
                return send_error(outbound, Some(request_id), error).await;
            }
            let _was_watching = watches.stop(command_id);
            outbound
                .normal_json(&ServerWatchState {
                    message_type: "command.unwatched",
                    request_id,
                    command_id,
                    watching: false,
                })
                .await
        }
        ClientMessage::CommandCancel {
            request_id,
            desktop_id,
            desktop_generation,
            command_id,
        } => {
            handle_command_cancel(
                outbound,
                state,
                principal,
                watches,
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            )
            .await
        }
        ClientMessage::LeaseGet {
            request_id,
            desktop_id,
            desktop_generation,
        } => {
            handle_lease_get(
                outbound,
                state,
                principal,
                request_id,
                desktop_id,
                desktop_generation,
            )
            .await
        }
        ClientMessage::LeaseAcquire { request_id, lease } => {
            if lease.protocol_version != negotiated_protocol {
                send_unsupported_message_version(outbound, request_id).await
            } else {
                handle_lease_acquire(outbound, state, principal, request_id, *lease).await
            }
        }
        ClientMessage::LeaseRenew { request_id, lease } => {
            if lease.protocol_version != negotiated_protocol {
                send_unsupported_message_version(outbound, request_id).await
            } else {
                handle_lease_renew(outbound, state, principal, request_id, *lease).await
            }
        }
        ClientMessage::LeaseRelease { request_id, lease } => {
            if lease.protocol_version != negotiated_protocol {
                send_unsupported_message_version(outbound, request_id).await
            } else {
                handle_lease_release(outbound, state, principal, request_id, *lease).await
            }
        }
        ClientMessage::EventsSubscribe {
            request_id,
            desktop_id,
            desktop_generation,
            topics,
            since_sequence,
        } => {
            handle_events_subscribe(
                outbound,
                state,
                principal,
                event_watch,
                request_id,
                desktop_id,
                desktop_generation,
                topics,
                since_sequence,
            )
            .await
        }
        ClientMessage::EventsUnsubscribe {
            request_id,
            desktop_id,
            desktop_generation,
        } => {
            handle_events_unsubscribe(
                outbound,
                state,
                principal,
                event_watch,
                request_id,
                desktop_id,
                desktop_generation,
            )
            .await
        }
    }
}

async fn handle_command_submit(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    watches: &mut WatchSet,
    request_id: RequestId,
    command: CommandEnvelope,
) -> Result<(), ()> {
    if request_id.as_uuid().is_nil() || command.request_id != request_id {
        return send_error(
            outbound,
            Some(request_id),
            WireFailure::new(
                ErrorCode::InvalidRequest,
                "The outer request identifier must exactly match the command envelope.",
            ),
        )
        .await;
    }
    if !principal.satisfies(crate::command_grant_requirement(&command.command)) {
        return send_permission_denied(outbound, request_id).await;
    }
    if command.validate().is_err() {
        return send_invalid_request(outbound, request_id).await;
    }
    if let Err(error) = validate_target(
        state,
        command.desktop_id,
        command.desktop_generation,
        command.command_id.as_uuid().is_nil(),
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    if !state.abuse.admit_command_submit(principal.id()) {
        return send_error(
            outbound,
            Some(request_id),
            WireFailure::new(
                ErrorCode::ResourceExhausted,
                "The authenticated principal command-submit rate limit was exceeded.",
            ),
        )
        .await;
    }
    let command_id = command.command_id;
    match state
        .control
        .submit_command(
            ControlRequestContext::new(principal.clone(), request_id),
            command,
        )
        .await
    {
        Ok(submission) => {
            publish_submission(
                outbound, state, principal, watches, request_id, command_id, submission,
            )
            .await
        }
        Err(error) => send_control_error(outbound, request_id, error).await,
    }
}

async fn publish_submission(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    watches: &mut WatchSet,
    request_id: RequestId,
    command_id: CommandId,
    submission: CommandSubmission,
) -> Result<(), ()> {
    if !valid_result(&submission.result, command_id) {
        return send_internal_error(outbound, request_id).await;
    }
    let terminal = submission.result.lifecycle().is_terminal();
    let disposition_valid = match submission.disposition {
        SubmissionDisposition::Accepted | SubmissionDisposition::ExistingInProgress => !terminal,
        SubmissionDisposition::ExistingTerminal => terminal,
    };
    if !disposition_valid {
        return send_internal_error(outbound, request_id).await;
    }
    if terminal {
        watches.stop(command_id);
        outbound
            .high_json(&ServerCommand {
                message_type: "command.result",
                request_id,
                result: &submission.result,
            })
            .await
    } else {
        outbound
            .normal_json(&ServerCommand {
                message_type: "command.accepted",
                request_id,
                result: &submission.result,
            })
            .await?;
        if !watches.start(
            state.clone(),
            principal.clone(),
            request_id,
            state.desktop_id,
            command_id,
            outbound.clone(),
        ) {
            return send_watch_capacity_error(outbound, request_id).await;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_command_watch(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    watches: &mut WatchSet,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    command_id: CommandId,
) -> Result<(), ()> {
    if let Err(error) = validate_observe_request(
        state,
        principal,
        request_id,
        desktop_id,
        desktop_generation,
        command_id,
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    watches.stop(command_id);
    match state
        .control
        .command_result(
            ControlRequestContext::new(principal.clone(), request_id),
            desktop_id,
            command_id,
        )
        .await
    {
        Ok(result) if valid_result(&result, command_id) => {
            if result.lifecycle().is_terminal() {
                watches.stop(command_id);
                outbound
                    .high_json(&ServerCommand {
                        message_type: "command.result",
                        request_id,
                        result: &result,
                    })
                    .await
            } else {
                outbound
                    .normal_json(&ServerCommand {
                        message_type: "command.progress",
                        request_id,
                        result: &result,
                    })
                    .await?;
                if !watches.start(
                    state.clone(),
                    principal.clone(),
                    request_id,
                    desktop_id,
                    command_id,
                    outbound.clone(),
                ) {
                    return send_watch_capacity_error(outbound, request_id).await;
                }
                Ok(())
            }
        }
        Ok(_) => send_internal_error(outbound, request_id).await,
        Err(error) => send_control_error(outbound, request_id, error).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_command_cancel(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    watches: &mut WatchSet,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    command_id: CommandId,
) -> Result<(), ()> {
    if request_id.as_uuid().is_nil()
        || command_id.as_uuid().is_nil()
        || !principal.has_command_cancellation_grant()
    {
        if !principal.has_command_cancellation_grant() {
            return send_permission_denied(outbound, request_id).await;
        }
        return send_invalid_request(outbound, request_id).await;
    }
    if let Err(error) = validate_target(state, desktop_id, desktop_generation, false) {
        return send_error(outbound, Some(request_id), error).await;
    }
    match state
        .control
        .cancel_command(
            ControlRequestContext::new(principal.clone(), request_id),
            desktop_id,
            command_id,
        )
        .await
    {
        Ok(CommandCancellation::AlreadyTerminal(result))
            if valid_result(&result, command_id) && result.lifecycle().is_terminal() =>
        {
            watches.stop(command_id);
            outbound
                .high_json(&ServerCommand {
                    message_type: "command.result",
                    request_id,
                    result: &result,
                })
                .await
        }
        Ok(CommandCancellation::Accepted(result)) if valid_result(&result, command_id) => {
            if result.lifecycle().is_terminal() {
                watches.stop(command_id);
                outbound
                    .high_json(&ServerCommand {
                        message_type: "command.result",
                        request_id,
                        result: &result,
                    })
                    .await
            } else {
                outbound
                    .normal_json(&ServerCommand {
                        message_type: "command.progress",
                        request_id,
                        result: &result,
                    })
                    .await?;
                if !watches.start(
                    state.clone(),
                    principal.clone(),
                    request_id,
                    desktop_id,
                    command_id,
                    outbound.clone(),
                ) {
                    return send_watch_capacity_error(outbound, request_id).await;
                }
                Ok(())
            }
        }
        Ok(_) => send_internal_error(outbound, request_id).await,
        Err(error) => send_control_error(outbound, request_id, error).await,
    }
}

async fn handle_lease_get(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), ()> {
    if !principal.has_grant(Grant::InputControl) {
        return send_permission_denied(outbound, request_id).await;
    }
    if request_id.as_uuid().is_nil() {
        return send_invalid_request(outbound, request_id).await;
    }
    if let Err(error) = validate_target(state, desktop_id, desktop_generation, false) {
        return send_error(outbound, Some(request_id), error).await;
    }
    let result = state
        .control
        .lease_state(
            ControlRequestContext::new(principal.clone(), request_id),
            desktop_id,
        )
        .await;
    publish_lease(outbound, request_id, desktop_id, desktop_generation, result).await
}

async fn handle_lease_acquire(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    request_id: RequestId,
    lease: LeaseAcquireRequest,
) -> Result<(), ()> {
    if let Err(error) = validate_lease_message(
        state,
        principal,
        request_id,
        lease.request_id,
        lease.desktop_id,
        lease.desktop_generation,
        lease.validate().is_ok(),
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    let desktop_id = lease.desktop_id;
    let generation = lease.desktop_generation;
    let result = state
        .control
        .acquire_lease(
            ControlRequestContext::new(principal.clone(), request_id),
            lease,
        )
        .await;
    publish_lease(outbound, request_id, desktop_id, generation, result).await
}

async fn handle_lease_renew(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    request_id: RequestId,
    lease: LeaseRenewRequest,
) -> Result<(), ()> {
    if let Err(error) = validate_lease_message(
        state,
        principal,
        request_id,
        lease.request_id,
        lease.desktop_id,
        lease.desktop_generation,
        lease.validate().is_ok(),
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    let desktop_id = lease.desktop_id;
    let generation = lease.desktop_generation;
    let result = state
        .control
        .renew_lease(
            ControlRequestContext::new(principal.clone(), request_id),
            lease,
        )
        .await;
    publish_lease(outbound, request_id, desktop_id, generation, result).await
}

async fn handle_lease_release(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    request_id: RequestId,
    lease: LeaseReleaseRequest,
) -> Result<(), ()> {
    if let Err(error) = validate_lease_message(
        state,
        principal,
        request_id,
        lease.request_id,
        lease.desktop_id,
        lease.desktop_generation,
        lease.validate().is_ok(),
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    let desktop_id = lease.desktop_id;
    let generation = lease.desktop_generation;
    let result = state
        .control
        .release_lease(
            ControlRequestContext::new(principal.clone(), request_id),
            lease,
        )
        .await;
    publish_lease(outbound, request_id, desktop_id, generation, result).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_events_subscribe(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    event_watch: &mut EventWatch,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    topics: Vec<EventTopic>,
    since_sequence: Option<u64>,
) -> Result<(), ()> {
    if !principal.has_grant(Grant::DesktopObserve) {
        return send_permission_denied(outbound, request_id).await;
    }
    let allow_accessibility = principal.has_grant(Grant::AccessibilityRead);
    if !allow_accessibility && topics.iter().any(is_accessibility_topic) {
        return send_permission_denied(outbound, request_id).await;
    }
    if request_id.as_uuid().is_nil()
        || topics.len() > MAX_EVENT_TOPICS
        || topics.iter().any(|topic| topic.validate().is_err())
        || topics.iter().collect::<BTreeSet<_>>().len() != topics.len()
    {
        return send_invalid_request(outbound, request_id).await;
    }
    if let Err(error) = validate_target(state, desktop_id, desktop_generation, false) {
        return send_error(outbound, Some(request_id), error).await;
    }
    let subscription = match state
        .control
        .subscribe_events(
            ControlRequestContext::new(principal.clone(), request_id),
            desktop_id,
            desktop_generation,
            since_sequence,
        )
        .await
    {
        Ok(subscription) => subscription,
        Err(error) => return send_control_error(outbound, request_id, error).await,
    };
    outbound
        .normal_json(&ServerEventSubscription {
            message_type: "events.subscribed",
            request_id,
            topics: &topics,
        })
        .await?;
    event_watch.start(
        subscription,
        EventDeliveryTarget {
            request_id,
            desktop_id,
            desktop_generation,
            topics,
            allow_accessibility,
        },
        outbound.clone(),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_events_unsubscribe(
    outbound: &OutboundQueues,
    state: &ApiState,
    principal: &Principal,
    event_watch: &mut EventWatch,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), ()> {
    if !principal.has_grant(Grant::DesktopObserve) {
        return send_permission_denied(outbound, request_id).await;
    }
    if let Err(error) = validate_target(
        state,
        desktop_id,
        desktop_generation,
        request_id.as_uuid().is_nil(),
    ) {
        return send_error(outbound, Some(request_id), error).await;
    }
    event_watch.stop();
    outbound
        .normal_json(&ServerEventUnsubscribed {
            message_type: "events.unsubscribed",
            request_id,
        })
        .await
}

async fn publish_lease(
    outbound: &OutboundQueues,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    result: Result<LeaseStateView, ControlPlaneError>,
) -> Result<(), ()> {
    match result {
        Ok(lease)
            if lease.validate().is_ok()
                && lease.desktop_id == desktop_id
                && lease.desktop_generation == desktop_generation =>
        {
            outbound
                .normal_json(&ServerLease {
                    message_type: "lease.state",
                    request_id,
                    lease: &lease,
                })
                .await
        }
        Ok(_) => send_internal_error(outbound, request_id).await,
        Err(error) => send_control_error(outbound, request_id, error).await,
    }
}

async fn deliver_events(
    mut subscription: EventSubscription,
    target: EventDeliveryTarget,
    outbound: OutboundQueues,
) {
    let EventDeliveryTarget {
        request_id,
        desktop_id,
        desktop_generation,
        topics,
        allow_accessibility,
    } = target;
    let replay = core::mem::replace(
        &mut subscription.replay,
        EventReplay::Events {
            latest_sequence: 0,
            events: Vec::new(),
        },
    );
    let latest_sequence = match replay {
        EventReplay::Events {
            latest_sequence,
            events,
        } => {
            for event in events {
                if !topic_matches(&topics, &event.topic, allow_accessibility) {
                    continue;
                }
                match outbound.droppable_json(&ServerEvent {
                    message_type: "event",
                    request_id,
                    event: &event,
                }) {
                    Ok(DroppableSend::Enqueued) => {}
                    Ok(DroppableSend::Dropped) => {
                        let _ignored = send_resync_required(
                            &outbound,
                            request_id,
                            desktop_id,
                            desktop_generation,
                            EventResyncReason::OutboundBackpressure,
                            event.sequence,
                            event.sequence,
                        )
                        .await;
                        return;
                    }
                    Err(()) => return,
                }
            }
            latest_sequence
        }
        EventReplay::ResyncRequired {
            reason,
            desktop_generation,
            dropped_through,
            latest_sequence,
        } => {
            let _ignored = send_resync_required(
                &outbound,
                request_id,
                desktop_id,
                desktop_generation,
                reason,
                dropped_through,
                latest_sequence,
            )
            .await;
            return;
        }
    };
    if outbound
        .normal_json(&ServerReplayComplete {
            message_type: "events.replay_complete",
            request_id,
            desktop_id,
            desktop_generation,
            through_sequence: latest_sequence,
        })
        .await
        .is_err()
    {
        return;
    }

    loop {
        match subscription.live.receive().await {
            LiveEvent::Event(event) => {
                if event.desktop_id != desktop_id
                    || event.desktop_generation != desktop_generation
                    || event.validate().is_err()
                {
                    let _ignored = send_resync_required(
                        &outbound,
                        request_id,
                        desktop_id,
                        event.desktop_generation,
                        EventResyncReason::GenerationChanged,
                        event.sequence,
                        event.sequence,
                    )
                    .await;
                    return;
                }
                if !topic_matches(&topics, &event.topic, allow_accessibility) {
                    continue;
                }
                match outbound.droppable_json(&ServerEvent {
                    message_type: "event",
                    request_id,
                    event: &event,
                }) {
                    Ok(DroppableSend::Enqueued) => {}
                    Ok(DroppableSend::Dropped) => {
                        let _ignored = send_resync_required(
                            &outbound,
                            request_id,
                            desktop_id,
                            desktop_generation,
                            EventResyncReason::OutboundBackpressure,
                            event.sequence,
                            event.sequence,
                        )
                        .await;
                        return;
                    }
                    Err(()) => return,
                }
            }
            LiveEvent::ResyncRequired {
                reason,
                desktop_generation,
                dropped_through,
                latest_sequence,
            } => {
                let _ignored = send_resync_required(
                    &outbound,
                    request_id,
                    desktop_id,
                    desktop_generation,
                    reason,
                    dropped_through,
                    latest_sequence,
                )
                .await;
                return;
            }
            LiveEvent::Closed => return,
        }
    }
}

fn topic_matches(topics: &[EventTopic], topic: &EventTopic, allow_accessibility: bool) -> bool {
    (allow_accessibility || !is_accessibility_topic(topic))
        && (topics.is_empty() || topics.iter().any(|candidate| candidate == topic))
}

fn is_accessibility_topic(topic: &EventTopic) -> bool {
    topic.as_str().starts_with("accessibility.")
}

#[allow(clippy::too_many_arguments)]
async fn send_resync_required(
    outbound: &OutboundQueues,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    reason: EventResyncReason,
    dropped_through: u64,
    latest_sequence: u64,
) -> Result<(), ()> {
    outbound
        .high_json(&ServerResyncRequired {
            message_type: "events.resync_required",
            request_id,
            desktop_id,
            desktop_generation,
            reason,
            dropped_through,
            latest_sequence,
        })
        .await
}

async fn watch_command(
    state: ApiState,
    principal: Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    command_id: CommandId,
    outbound: OutboundQueues,
) {
    loop {
        let started = tokio::time::Instant::now();
        let result = state
            .control
            .wait_command(
                ControlRequestContext::new(principal.clone(), request_id),
                desktop_id,
                command_id,
                WATCH_WAIT,
            )
            .await;
        match result {
            Ok(CommandWait::Terminal(result))
                if valid_result(&result, command_id) && result.lifecycle().is_terminal() =>
            {
                let _ignored = outbound
                    .high_json(&ServerCommand {
                        message_type: "command.result",
                        request_id,
                        result: &result,
                    })
                    .await;
                return;
            }
            Ok(CommandWait::TimedOut(result)) if valid_result(&result, command_id) => {
                if result.lifecycle().is_terminal() {
                    let _ignored = outbound
                        .high_json(&ServerCommand {
                            message_type: "command.result",
                            request_id,
                            result: &result,
                        })
                        .await;
                    return;
                }
                if outbound
                    .progress_json(&ServerCommand {
                        message_type: "command.progress",
                        request_id,
                        result: &result,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {
                let _ignored = send_internal_error(&outbound, request_id).await;
                return;
            }
            Err(error) => {
                let _ignored = send_control_error(&outbound, request_id, error).await;
                return;
            }
        }
        if started.elapsed() < FAST_WATCH_RETRY_DELAY {
            tokio::time::sleep(FAST_WATCH_RETRY_DELAY).await;
        }
    }
}

fn validate_observe_request(
    state: &ApiState,
    principal: &Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    command_id: CommandId,
) -> Result<(), WireFailure> {
    if !principal.has_grant(Grant::DesktopObserve) {
        return Err(permission_failure());
    }
    validate_target(
        state,
        desktop_id,
        desktop_generation,
        request_id.as_uuid().is_nil() || command_id.as_uuid().is_nil(),
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_lease_message(
    state: &ApiState,
    principal: &Principal,
    outer_request_id: RequestId,
    inner_request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    shape_valid: bool,
) -> Result<(), WireFailure> {
    if !principal.has_grant(Grant::InputControl) {
        return Err(permission_failure());
    }
    validate_target(
        state,
        desktop_id,
        desktop_generation,
        !shape_valid || outer_request_id.as_uuid().is_nil() || outer_request_id != inner_request_id,
    )
}

fn validate_target(
    state: &ApiState,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    invalid_shape: bool,
) -> Result<(), WireFailure> {
    if invalid_shape || desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
        return Err(WireFailure::new(
            ErrorCode::InvalidRequest,
            "The message contains an invalid identifier or protocol shape.",
        ));
    }
    if desktop_id != state.desktop_id {
        return Err(WireFailure::new(
            ErrorCode::NotFound,
            "The requested desktop does not exist.",
        ));
    }
    let readiness = state.readiness.snapshot();
    if !readiness.is_ready() {
        return Err(WireFailure::new(
            ErrorCode::CapabilityUnavailable,
            "The desktop is not ready to accept this operation.",
        ));
    }
    match readiness.desktop_generation {
        Some(current) if current == desktop_generation => Ok(()),
        current => Err(WireFailure::stale(current)),
    }
}

fn valid_result(result: &CommandResult, command_id: CommandId) -> bool {
    result.command_id() == command_id && result.validate().is_ok()
}

fn permission_failure() -> WireFailure {
    WireFailure::new(
        ErrorCode::PermissionDenied,
        "The authenticated principal lacks the required capability.",
    )
}

fn control_failure(error: ControlPlaneError) -> WireFailure {
    match error {
        ControlPlaneError::InvalidRequest => WireFailure::new(
            ErrorCode::InvalidRequest,
            "The request does not match the versioned protocol shape.",
        ),
        ControlPlaneError::PermissionDenied => permission_failure(),
        ControlPlaneError::NotFound => WireFailure::new(
            ErrorCode::NotFound,
            "The requested control resource does not exist.",
        ),
        ControlPlaneError::StaleReference { current_generation } => {
            WireFailure::stale(current_generation)
        }
        ControlPlaneError::CommandIdConflict => WireFailure::new(
            ErrorCode::CommandIdConflict,
            "The command identifier is already bound to different command content.",
        ),
        ControlPlaneError::LeaseConflict => WireFailure::new(
            ErrorCode::LeaseConflict,
            "The lease operation conflicts with current controller state.",
        ),
        ControlPlaneError::ResourceExhausted => WireFailure::new(
            ErrorCode::ResourceExhausted,
            "A bounded control queue or quota is currently full.",
        ),
        ControlPlaneError::CapabilityUnavailable => WireFailure::new(
            ErrorCode::CapabilityUnavailable,
            "The required desktop capability is unavailable.",
        ),
        ControlPlaneError::UnsupportedByTarget | ControlPlaneError::CancellationConflict => {
            WireFailure::new(
                ErrorCode::UnsupportedByTarget,
                "The target cannot safely perform this operation.",
            )
        }
        ControlPlaneError::Internal => WireFailure::new(
            ErrorCode::Internal,
            "The server could not complete the request safely.",
        ),
    }
}

async fn send_protocol_error(
    outbound: &OutboundQueues,
    request_id: Option<RequestId>,
    detail: &'static str,
) -> Result<(), ()> {
    send_error(
        outbound,
        request_id,
        WireFailure::new(ErrorCode::InvalidRequest, detail),
    )
    .await?;
    outbound
        .high_message(close(1007, "invalid application message"))
        .await?;
    Err(())
}

async fn send_error(
    outbound: &OutboundQueues,
    request_id: Option<RequestId>,
    failure: WireFailure,
) -> Result<(), ()> {
    outbound
        .high_json(&ServerError {
            message_type: "error",
            request_id,
            code: failure.code,
            detail: failure.detail,
            desktop_generation: failure.desktop_generation,
        })
        .await
}

async fn send_message_rate_exhausted(outbound: &OutboundQueues, message: &str) -> Result<(), ()> {
    let request_id = serde_json::from_str::<MessageHeader>(message)
        .ok()
        .and_then(|header| header.request_id);
    send_error(
        outbound,
        request_id,
        WireFailure::new(
            ErrorCode::ResourceExhausted,
            "The per-session application-message rate limit was exceeded.",
        ),
    )
    .await?;
    outbound
        .high_message(close(1008, "message rate exceeded"))
        .await
}

async fn send_control_error(
    outbound: &OutboundQueues,
    request_id: RequestId,
    error: ControlPlaneError,
) -> Result<(), ()> {
    send_error(outbound, Some(request_id), control_failure(error)).await
}

async fn send_invalid_request(outbound: &OutboundQueues, request_id: RequestId) -> Result<(), ()> {
    send_error(
        outbound,
        Some(request_id),
        WireFailure::new(
            ErrorCode::InvalidRequest,
            "The message contains an invalid identifier or protocol shape.",
        ),
    )
    .await
}

async fn send_unsupported_message_version(
    outbound: &OutboundQueues,
    request_id: RequestId,
) -> Result<(), ()> {
    send_error(
        outbound,
        Some(request_id),
        WireFailure::new(
            ErrorCode::UnsupportedVersion,
            "The embedded request version does not match the negotiated protocol version.",
        ),
    )
    .await
}

async fn send_permission_denied(
    outbound: &OutboundQueues,
    request_id: RequestId,
) -> Result<(), ()> {
    send_error(outbound, Some(request_id), permission_failure()).await
}

async fn send_internal_error(outbound: &OutboundQueues, request_id: RequestId) -> Result<(), ()> {
    send_error(
        outbound,
        Some(request_id),
        control_failure(ControlPlaneError::Internal),
    )
    .await
}

async fn send_watch_capacity_error(
    outbound: &OutboundQueues,
    request_id: RequestId,
) -> Result<(), ()> {
    send_error(
        outbound,
        Some(request_id),
        WireFailure::new(
            ErrorCode::ResourceExhausted,
            "The per-session command watch limit is full.",
        ),
    )
    .await
}

async fn send_serialized<T: Serialize>(
    outbound: &mpsc::Sender<Message>,
    value: &T,
) -> Result<(), ()> {
    outbound.send(encode_message(value)?).await.map_err(|_| ())
}

fn encode_message<T: Serialize>(value: &T) -> Result<Message, ()> {
    serde_json::to_string(value)
        .map(|encoded| Message::Text(encoded.into()))
        .map_err(|_| ())
}

fn serialize_u64_string<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_str(value)
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: Utf8Bytes::from_static(reason),
    }))
}

fn websocket_read_error_close(error: &axum::Error) -> Option<Message> {
    let error = std::error::Error::source(error)?.downcast_ref::<tungstenite::Error>()?;
    match error {
        tungstenite::Error::Utf8(_) => Some(close(1007, "invalid UTF-8")),
        tungstenite::Error::Capacity(_) => Some(close(1009, "message too large")),
        tungstenite::Error::Protocol(_) => Some(close(1002, "protocol error")),
        _ => None,
    }
}

fn welcome_limits(limits: crate::TransportLimits) -> Option<WelcomeLimits> {
    Some(WelcomeLimits {
        max_message_bytes: u32::try_from(limits.max_body_bytes()).ok()?,
        heartbeat_ms: u32::try_from(limits.ws_heartbeat().as_millis()).ok()?,
        normal_outbound_capacity: u32::try_from(limits.ws_outbound_capacity()).ok()?,
        reserved_outbound_capacity: u32::try_from(limits.ws_high_priority_capacity()).ok()?,
        max_command_watches: u32::try_from(MAX_SESSION_COMMAND_WATCHES).ok()?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandshakeError {
    InvalidMessage,
    UnsupportedVersion,
}

impl HandshakeError {
    const fn safe_detail(self) -> &'static str {
        match self {
            Self::InvalidMessage => "The first message must be a valid bounded client.hello.",
            Self::UnsupportedVersion => {
                "The client and server do not share a supported protocol version."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::get,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use tower::ServiceExt;
    use xenoteer_protocol::{
        ApplicationId, ApplicationLaunchCommand, Command, DesktopProbeCommand, EventResumeRequest,
        LaunchId, Point, PointerCurve, PointerMoveCommand, ProcessRef, ProcessTerminateCommand,
        WebSocketClientDescriptor,
    };

    use super::*;
    use crate::{
        AllowedOrigins, Authentication, ControlFuture, ControlPlane, DesktopReadiness,
        ReadinessHandle, ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider,
        TransportLimits, api_router_with_control, control::UnavailableControlPlane,
    };

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[derive(Debug, Default)]
    struct CountingControl {
        calls: AtomicUsize,
    }

    impl CountingControl {
        fn unavailable<'a, T>(&'a self) -> ControlFuture<'a, Result<T, ControlPlaneError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
        }
    }

    impl ControlPlane for CountingControl {
        fn lease_state<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.unavailable()
        }

        fn acquire_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            _: LeaseAcquireRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.unavailable()
        }

        fn renew_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            _: LeaseRenewRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.unavailable()
        }

        fn release_lease<'a>(
            &'a self,
            _: ControlRequestContext,
            _: LeaseReleaseRequest,
        ) -> ControlFuture<'a, Result<LeaseStateView, ControlPlaneError>> {
            self.unavailable()
        }

        fn submit_command<'a>(
            &'a self,
            _: ControlRequestContext,
            _: CommandEnvelope,
        ) -> ControlFuture<'a, Result<CommandSubmission, ControlPlaneError>> {
            self.unavailable()
        }

        fn command_result<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            _: CommandId,
        ) -> ControlFuture<'a, Result<CommandResult, ControlPlaneError>> {
            self.unavailable()
        }

        fn wait_command<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            _: CommandId,
            _: Duration,
        ) -> ControlFuture<'a, Result<CommandWait, ControlPlaneError>> {
            self.unavailable()
        }

        fn cancel_command<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            _: CommandId,
        ) -> ControlFuture<'a, Result<CommandCancellation, ControlPlaneError>> {
            self.unavailable()
        }

        fn subscribe_events<'a>(
            &'a self,
            _: ControlRequestContext,
            _: DesktopId,
            _: DesktopGeneration,
            _: Option<u64>,
        ) -> ControlFuture<'a, Result<EventSubscription, ControlPlaneError>> {
            self.unavailable()
        }
    }

    fn test_state(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Result<ApiState, xenoteer_protocol::CapabilityReportError> {
        let limits = TransportLimits::default();
        Ok(ApiState {
            readiness: ReadinessHandle::new(ReadinessSnapshot::new(
                DesktopReadiness::Ready,
                Some(generation),
                None::<String>,
            )),
            desktop_id,
            capabilities: Arc::new(StaticCapabilityProvider::empty()?),
            limits,
            origins: AllowedOrigins::default(),
            control: Arc::new(UnavailableControlPlane),
            observation: Arc::new(crate::observation::UnavailableObservationPlane),
            abuse: crate::abuse::AbuseControls::new(),
            long_polls: crate::limits::LongPollAdmission::new(limits),
        })
    }

    fn test_state_with_control(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        control: Arc<CountingControl>,
    ) -> Result<ApiState, xenoteer_protocol::CapabilityReportError> {
        let mut state = test_state(desktop_id, generation)?;
        state.control = control;
        Ok(state)
    }

    fn test_process_ref(generation: DesktopGeneration) -> ProcessRef {
        ProcessRef {
            desktop_generation: generation,
            pid: 42,
            proc_start_ticks: 7,
            launch_id: LaunchId::new(),
        }
    }

    fn submit_message(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        command: Command,
    ) -> Result<ClientMessage, Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let envelope = if command.requires_control_lease() {
            CommandEnvelope::new_with_lease(
                ProtocolVersion::V1_0,
                request_id,
                command_id,
                desktop_id,
                generation,
                xenoteer_protocol::ControlLeaseId::new(),
                command,
            )?
        } else {
            CommandEnvelope::new(
                ProtocolVersion::V1_0,
                request_id,
                command_id,
                desktop_id,
                generation,
                command,
            )?
        };
        Ok(ClientMessage::CommandSubmit {
            request_id,
            command: Box::new(envelope),
        })
    }

    fn message_request_id(message: &ClientMessage) -> RequestId {
        match message {
            ClientMessage::Ping { request_id, .. }
            | ClientMessage::CommandSubmit { request_id, .. }
            | ClientMessage::CommandWatch { request_id, .. }
            | ClientMessage::CommandUnwatch { request_id, .. }
            | ClientMessage::CommandCancel { request_id, .. }
            | ClientMessage::LeaseGet { request_id, .. }
            | ClientMessage::LeaseAcquire { request_id, .. }
            | ClientMessage::LeaseRenew { request_id, .. }
            | ClientMessage::LeaseRelease { request_id, .. }
            | ClientMessage::EventsSubscribe { request_id, .. }
            | ClientMessage::EventsUnsubscribe { request_id, .. } => *request_id,
        }
    }

    async fn assert_permission_denied_before_control(
        state: &ApiState,
        principal: &Principal,
        control: &CountingControl,
        message: &ClientMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let request_id = message_request_id(message);
        let calls_before = control.calls.load(Ordering::SeqCst);
        let (outbound, mut receiver) = OutboundQueues::bounded(4, 4);
        let mut watches = WatchSet::new();
        let mut event_watch = EventWatch::new();
        handle_text(
            &outbound,
            state,
            principal,
            &mut watches,
            &mut event_watch,
            ProtocolVersion::V1_0,
            &serde_json::to_string(message)?,
        )
        .await
        .map_err(|()| std::io::Error::other("authorization denial closed the session"))?;
        let response = receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("missing authorization error"))?;
        let Message::Text(response) = response else {
            return Err(std::io::Error::other("authorization denial was not JSON text").into());
        };
        let response: serde_json::Value = serde_json::from_str(response.as_str())?;
        assert_eq!(response["type"], "error");
        assert_eq!(response["code"], "permission_denied");
        assert_eq!(response["request_id"], request_id.to_string());
        assert_eq!(control.calls.load(Ordering::SeqCst), calls_before);
        assert!(watches.owned.is_empty());
        Ok(())
    }

    async fn write_masked_text(
        stream: &mut TcpStream,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut frame = Vec::with_capacity(payload.len().saturating_add(16));
        frame.push(0x81);
        if payload.len() < 126 {
            frame.push(0x80 | u8::try_from(payload.len())?);
        } else if payload.len() <= usize::from(u16::MAX) {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&u16::try_from(payload.len())?.to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&u64::try_from(payload.len())?.to_be_bytes());
        }
        let mask = [0x13, 0x57, 0x9b, 0xdf];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| *byte ^ mask[index % mask.len()]),
        );
        stream.write_all(&frame).await?;
        Ok(())
    }

    async fn read_server_frame(
        stream: &mut TcpStream,
    ) -> Result<(u8, Vec<u8>), Box<dyn std::error::Error>> {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).await?;
        if header[1] & 0x80 != 0 {
            return Err(std::io::Error::other("server WebSocket frame was masked").into());
        }
        let length = match header[1] & 0x7f {
            length @ 0..=125 => usize::from(length),
            126 => {
                let mut encoded = [0_u8; 2];
                stream.read_exact(&mut encoded).await?;
                usize::from(u16::from_be_bytes(encoded))
            }
            127 => {
                let mut encoded = [0_u8; 8];
                stream.read_exact(&mut encoded).await?;
                usize::try_from(u64::from_be_bytes(encoded))?
            }
            _ => unreachable!("a masked seven-bit frame length cannot exceed 127"),
        };
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await?;
        Ok((header[0] & 0x0f, payload))
    }

    struct ClosedLiveEvents;

    impl crate::LiveEventReceiver for ClosedLiveEvents {
        fn receive<'a>(&'a mut self) -> crate::ControlFuture<'a, LiveEvent> {
            Box::pin(async { LiveEvent::Closed })
        }
    }

    #[test]
    fn public_server_event_types_match_private_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let connection_id = ConnectionId::new();
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let event = SequencedEvent {
            desktop_id,
            desktop_generation: generation,
            sequence: 7,
            topic: EventTopic::new("command.lifecycle")?,
            payload: serde_json::json!({"command_lifecycle": "running"}),
        };

        let private_welcome = ServerWelcome {
            message_type: "server.welcome",
            protocol: ProtocolVersion::V1_0,
            connection_id,
            principal: WelcomePrincipal {
                id: "observer".to_owned(),
                capabilities: vec!["desktop.observe".to_owned()],
            },
            desktop: WelcomeDesktop {
                id: desktop_id,
                generation: Some(generation),
                state: DesktopReadiness::Ready,
            },
            limits: WelcomeLimits {
                max_message_bytes: 1_048_576,
                heartbeat_ms: 15_000,
                normal_outbound_capacity: 32,
                reserved_outbound_capacity: 8,
                max_command_watches: 256,
            },
            resume: WelcomeResume {
                status: EventResumeStatus::Replayed,
            },
        };
        let public_welcome = xenoteer_protocol::WebSocketServerMessage::Welcome {
            protocol: ProtocolVersion::V1_0,
            connection_id,
            principal: xenoteer_protocol::WelcomePrincipal {
                id: "observer".to_owned(),
                capabilities: vec!["desktop.observe".to_owned()],
            },
            desktop: xenoteer_protocol::WelcomeDesktop {
                id: desktop_id,
                generation: Some(generation),
                state: xenoteer_protocol::WelcomeDesktopState::Ready,
            },
            limits: xenoteer_protocol::WelcomeLimits {
                max_message_bytes: 1_048_576,
                heartbeat_ms: 15_000,
                normal_outbound_capacity: 32,
                reserved_outbound_capacity: 8,
                max_command_watches: 256,
            },
            resume: xenoteer_protocol::WelcomeResume {
                status: EventResumeStatus::Replayed,
            },
        };
        assert_eq!(
            serde_json::to_value(private_welcome)?,
            serde_json::to_value(public_welcome)?
        );

        assert_eq!(
            serde_json::to_value(ServerEvent {
                message_type: "event",
                request_id,
                event: &event,
            })?,
            serde_json::to_value(xenoteer_protocol::WebSocketServerMessage::Event {
                request_id,
                event: event.clone(),
            })?
        );
        assert_eq!(
            serde_json::to_value(ServerReplayComplete {
                message_type: "events.replay_complete",
                request_id,
                desktop_id,
                desktop_generation: generation,
                through_sequence: 7,
            })?,
            serde_json::to_value(
                xenoteer_protocol::WebSocketServerMessage::EventsReplayComplete {
                    request_id,
                    desktop_id,
                    desktop_generation: generation,
                    through_sequence: 7,
                }
            )?
        );
        assert_eq!(
            serde_json::to_value(ServerResyncRequired {
                message_type: "events.resync_required",
                request_id,
                desktop_id,
                desktop_generation: generation,
                reason: EventResyncReason::HistoryLost,
                dropped_through: 4,
                latest_sequence: 7,
            })?,
            serde_json::to_value(
                xenoteer_protocol::WebSocketServerMessage::EventsResyncRequired {
                    request_id,
                    desktop_id,
                    desktop_generation: generation,
                    reason: EventResyncReason::HistoryLost,
                    dropped_through: 4,
                    latest_sequence: 7,
                }
            )?
        );

        let private_draining = serde_json::to_value(ServerDraining {
            message_type: "server.draining",
            desktop_id,
            desktop_generation: Some(generation),
            reason_code: None,
        })?;
        let public_draining =
            serde_json::to_value(xenoteer_protocol::WebSocketServerMessage::ServerDraining {
                desktop_id,
                desktop_generation: Some(generation),
                reason_code: None,
            })?;
        assert_eq!(private_draining, public_draining);
        assert!(private_draining.get("reason_code").is_none());

        let private_error = serde_json::to_value(ServerError {
            message_type: "error",
            request_id: Some(request_id),
            code: ErrorCode::InvalidRequest,
            detail: "safe detail",
            desktop_generation: None,
        })?;
        let public_error =
            serde_json::to_value(xenoteer_protocol::WebSocketServerMessage::Error {
                request_id: Some(request_id),
                code: ErrorCode::InvalidRequest,
                detail: "safe detail".to_owned(),
                desktop_generation: None,
            })?;
        assert_eq!(private_error, public_error);
        assert!(private_error.get("desktop_generation").is_none());
        Ok(())
    }

    #[test]
    fn websocket_parser_errors_map_to_rfc_close_codes() {
        let utf8 = axum::Error::new(tungstenite::Error::Utf8("invalid".to_owned()));
        assert!(matches!(
            websocket_read_error_close(&utf8),
            Some(Message::Close(Some(frame))) if frame.code == 1007
        ));

        let capacity = axum::Error::new(tungstenite::Error::Capacity(
            tungstenite::error::CapacityError::MessageTooLong {
                size: 1_048_577,
                max_size: 1_048_576,
            },
        ));
        assert!(matches!(
            websocket_read_error_close(&capacity),
            Some(Message::Close(Some(frame))) if frame.code == 1009
        ));
    }

    #[test]
    fn origin_policy_is_exact_and_sdk_omission_is_allowed() -> Result<(), OriginPolicyError> {
        let policy = AllowedOrigins::exact(["https://viewer.example".to_owned()])?;
        assert!(policy.permits(&HeaderMap::new()));
        let mut allowed = HeaderMap::new();
        allowed.insert(
            header::ORIGIN,
            "https://viewer.example"
                .parse()
                .map_err(|_| OriginPolicyError::InvalidOrigin)?,
        );
        assert!(policy.permits(&allowed));
        let mut denied = HeaderMap::new();
        denied.insert(
            header::ORIGIN,
            "https://evil.example"
                .parse()
                .map_err(|_| OriginPolicyError::InvalidOrigin)?,
        );
        assert!(!policy.permits(&denied));
        for invalid in [
            "https://viewer.example/path",
            "https://user@viewer.example",
            "https://viewer.example?query",
            "file://viewer.example",
        ] {
            assert_eq!(
                AllowedOrigins::exact([invalid.to_owned()]),
                Err(OriginPolicyError::InvalidOrigin)
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn full_websocket_capacity_returns_structured_429_before_upgrade()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = test_state(DesktopId::new(), DesktopGeneration::new())?;
        let permits = (0..64)
            .map(|_| state.abuse.try_acquire_websocket())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| std::io::Error::other("could not fill WebSocket capacity"))?;
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let request_id = RequestId::new();
        let application = Router::new().route("/", get(upgrade)).with_state(state);
        let mut request = Request::get("/").body(Body::empty())?;
        request.extensions_mut().insert(principal);
        request.extensions_mut().insert(request_id);
        let response = application.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), 4_096).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["code"], "resource_exhausted");
        drop(permits);
        Ok(())
    }

    #[tokio::test]
    async fn session_message_exhaustion_reserves_error_before_policy_close()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let (outbound, mut receiver) = OutboundQueues::bounded(1, 2);
        let message = format!(r#"{{"type":"unknown","request_id":"{request_id}"}}"#);
        send_message_rate_exhausted(&outbound, &message)
            .await
            .map_err(|()| std::io::Error::other("reserved queue unexpectedly closed"))?;

        let Some(Message::Text(error)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing reserved error message").into());
        };
        let error: serde_json::Value = serde_json::from_str(&error)?;
        assert_eq!(error["code"], "resource_exhausted");
        assert_eq!(error["request_id"], request_id.to_string());

        let Some(Message::Close(Some(frame))) = receiver.recv().await else {
            return Err(std::io::Error::other("missing reserved policy close").into());
        };
        assert_eq!(frame.code, 1008);
        assert_eq!(frame.reason, "message rate exceeded");
        Ok(())
    }

    #[test]
    fn hello_rejects_unknown_fields_and_unsupported_versions() {
        let id = RequestId::new();
        let valid = format!(
            r#"{{"type":"client.hello","request_id":"{id}","protocol":{{"major":1,"min_minor":0,"max_minor":0}},"client":{{"name":"sdk","version":"1"}},"resume":null}}"#
        );
        assert!(parse_hello(&valid).is_ok());
        assert!(
            parse_hello(&valid.replace("\"resume\":null", "\"resume\":null,\"typo\":true"))
                .is_err()
        );
        assert!(parse_hello(&valid.replace("\"major\":1", "\"major\":2")).is_err());
    }

    #[test]
    fn control_message_parser_is_strict_and_preserves_request_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let encoded = format!(
            r#"{{"type":"command.watch","request_id":"{request_id}","desktop_id":"{desktop_id}","desktop_generation":"{generation}","command_id":"{command_id}"}}"#
        );
        let message: ClientMessage = serde_json::from_str(&encoded)?;
        assert!(matches!(
            message,
            ClientMessage::CommandWatch {
                request_id: parsed,
                command_id: parsed_command,
                ..
            } if parsed == request_id && parsed_command == command_id
        ));
        assert!(
            serde_json::from_str::<ClientMessage>(&encoded.replace('}', ",\"typo\":true}"))
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn reserved_queue_preempts_full_normal_queue() -> Result<(), Box<dyn std::error::Error>> {
        let (outbound, mut receiver) = OutboundQueues::bounded(1, 1);
        outbound
            .normal
            .send(Message::Text("progress".into()))
            .await?;
        outbound.high.send(Message::Text("terminal".into())).await?;
        let first = receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("missing reserved message"))?;
        assert!(matches!(first, Message::Text(text) if text.as_str() == "terminal"));
        let dropped = ServerPong {
            message_type: "server.pong",
            request_id: RequestId::new(),
            nonce: "drop".to_owned(),
        };
        assert_eq!(outbound.progress_json(&dropped), Ok(()));
        Ok(())
    }

    #[tokio::test]
    async fn topic_filter_preserves_authoritative_global_sequence_and_replay_completion()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let selected = EventTopic::new("command.lifecycle")?;
        let ignored = EventTopic::new("action.lifecycle")?;
        let event = |sequence, topic| SequencedEvent {
            desktop_id,
            desktop_generation: generation,
            sequence,
            topic,
            payload: serde_json::json!({"safe": true}),
        };
        let subscription = EventSubscription {
            replay: EventReplay::Events {
                latest_sequence: 3,
                events: vec![
                    event(1, ignored),
                    event(2, selected.clone()),
                    event(3, EventTopic::new("process.lifecycle")?),
                ],
            },
            live: Box::new(ClosedLiveEvents),
        };
        let (outbound, mut receiver) = OutboundQueues::bounded(4, 2);
        deliver_events(
            subscription,
            EventDeliveryTarget {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                topics: vec![selected],
                allow_accessibility: false,
            },
            outbound,
        )
        .await;

        let Some(Message::Text(event)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing filtered replay event").into());
        };
        let event: serde_json::Value = serde_json::from_str(&event)?;
        assert_eq!(event["event"]["sequence"], "2");
        let Some(Message::Text(complete)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing replay completion").into());
        };
        let complete: serde_json::Value = serde_json::from_str(&complete)?;
        assert_eq!(complete["type"], "events.replay_complete");
        assert_eq!(complete["through_sequence"], "3");
        Ok(())
    }

    #[test]
    fn accessibility_topics_are_filtered_from_catch_all_without_the_read_grant()
    -> Result<(), Box<dyn std::error::Error>> {
        let semantic = EventTopic::new("accessibility.object.changed")?;
        let ordinary = EventTopic::new("window.lifecycle")?;
        assert!(!topic_matches(&[], &semantic, false));
        assert!(topic_matches(&[], &semantic, true));
        assert!(topic_matches(&[], &ordinary, false));
        assert!(!topic_matches(
            std::slice::from_ref(&semantic),
            &ordinary,
            true
        ));
        assert!(topic_matches(
            &[semantic],
            &EventTopic::new("accessibility.object.changed")?,
            true
        ));
        Ok(())
    }

    #[tokio::test]
    async fn explicit_accessibility_event_subscription_requires_the_read_grant()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let control = Arc::new(CountingControl::default());
        let state = test_state_with_control(desktop_id, generation, Arc::clone(&control))?;
        let principal = Principal::new("desktop-observer", [Grant::DesktopObserve])?;
        let message = ClientMessage::EventsSubscribe {
            request_id: RequestId::new(),
            desktop_id,
            desktop_generation: generation,
            topics: vec![EventTopic::new("accessibility.object.changed")?],
            since_sequence: None,
        };
        assert_permission_denied_before_control(&state, &principal, &control, &message).await
    }

    #[tokio::test]
    async fn outbound_event_drop_uses_reserved_resync_and_ends_subscription()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let event = SequencedEvent {
            desktop_id,
            desktop_generation: generation,
            sequence: 4,
            topic: EventTopic::new("command.lifecycle")?,
            payload: serde_json::json!({"safe": true}),
        };
        let subscription = EventSubscription {
            replay: EventReplay::Events {
                latest_sequence: 4,
                events: vec![event],
            },
            live: Box::new(ClosedLiveEvents),
        };
        let (outbound, mut receiver) = OutboundQueues::bounded(1, 1);
        outbound
            .normal
            .send(Message::Text("occupied".into()))
            .await?;
        deliver_events(
            subscription,
            EventDeliveryTarget {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                topics: Vec::new(),
                allow_accessibility: false,
            },
            outbound,
        )
        .await;
        let Some(Message::Text(resync)) = receiver.high.recv().await else {
            return Err(std::io::Error::other("missing reserved resync").into());
        };
        let resync: serde_json::Value = serde_json::from_str(&resync)?;
        assert_eq!(resync["type"], "events.resync_required");
        assert_eq!(resync["reason"], "outbound_backpressure");
        Ok(())
    }

    #[tokio::test]
    async fn initial_draining_snapshot_orders_notice_before_1001_close()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let state = test_state(desktop_id, generation)?;
        state.readiness.transition(crate::ReadinessSnapshot::new(
            crate::DesktopReadiness::Draining,
            Some(generation),
            Some("test_draining"),
        ));
        let snapshot = state.readiness.snapshot();
        let (outbound, mut receiver) = OutboundQueues::bounded(1, 2);
        let mut watches = WatchSet::new();
        drain_session(&outbound, &mut watches, &state, &snapshot).await;

        let Some(Message::Text(notice)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing draining notice").into());
        };
        let notice: serde_json::Value = serde_json::from_str(&notice)?;
        assert_eq!(notice["type"], "server.draining");
        let Some(Message::Close(Some(frame))) = receiver.recv().await else {
            return Err(std::io::Error::other("missing draining close").into());
        };
        assert_eq!(frame.code, 1001);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_message_uses_reserved_error_then_close()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let state = test_state(desktop_id, generation)?;
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let (outbound, mut receiver) = OutboundQueues::bounded(4, 4);
        let mut watches = WatchSet::new();
        let mut event_watch = EventWatch::new();
        assert_eq!(
            handle_text(
                &outbound,
                &state,
                &principal,
                &mut watches,
                &mut event_watch,
                ProtocolVersion::V1_0,
                "not-json",
            )
            .await,
            Err(())
        );
        let error = receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("missing WebSocket error"))?;
        let Message::Text(error) = error else {
            return Err(std::io::Error::other("first response was not text").into());
        };
        let error: serde_json::Value = serde_json::from_str(error.as_str())?;
        assert_eq!(error["code"], "invalid_request");
        let close = receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("missing WebSocket close"))?;
        assert!(matches!(close, Message::Close(Some(frame)) if frame.code == 1007));
        Ok(())
    }

    #[tokio::test]
    async fn command_submit_rejects_mismatched_outer_request_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let outer_request_id = RequestId::new();
        let command = CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            desktop_id,
            generation,
            Command::DesktopProbe(xenoteer_protocol::DesktopProbeCommand {}),
        )?;
        let text = serde_json::json!({
            "type": "command.submit",
            "request_id": outer_request_id,
            "command": command,
        })
        .to_string();
        let state = test_state(desktop_id, generation)?;
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let (outbound, mut receiver) = OutboundQueues::bounded(4, 4);
        let mut watches = WatchSet::new();
        let mut event_watch = EventWatch::new();
        assert_eq!(
            handle_text(
                &outbound,
                &state,
                &principal,
                &mut watches,
                &mut event_watch,
                ProtocolVersion::V1_0,
                &text,
            )
            .await,
            Ok(())
        );
        let error = receiver
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("missing mismatch error"))?;
        let Message::Text(error) = error else {
            return Err(std::io::Error::other("mismatch response was not text").into());
        };
        let error: serde_json::Value = serde_json::from_str(error.as_str())?;
        assert_eq!(error["request_id"], outer_request_id.to_string());
        assert_eq!(error["code"], "invalid_request");
        assert!(watches.owned.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn embedded_minor_must_match_negotiation_without_closing_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let request_id = RequestId::new();
        let command = CommandEnvelope::new(
            ProtocolVersion::new(1, 1),
            request_id,
            CommandId::new(),
            desktop_id,
            generation,
            Command::DesktopProbe(xenoteer_protocol::DesktopProbeCommand {}),
        )?;
        let message = ClientMessage::CommandSubmit {
            request_id,
            command: Box::new(command),
        };
        let control = Arc::new(CountingControl::default());
        let state = test_state_with_control(desktop_id, generation, Arc::clone(&control))?;
        let principal = Principal::new("observer", [Grant::DesktopObserve])?;
        let (outbound, mut receiver) = OutboundQueues::bounded(4, 4);
        let mut watches = WatchSet::new();
        let mut event_watch = EventWatch::new();

        handle_text(
            &outbound,
            &state,
            &principal,
            &mut watches,
            &mut event_watch,
            ProtocolVersion::V1_0,
            &serde_json::to_string(&message)?,
        )
        .await
        .map_err(|()| std::io::Error::other("version rejection closed the session"))?;
        let Some(Message::Text(error)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing unsupported-version response").into());
        };
        let error: serde_json::Value = serde_json::from_str(error.as_str())?;
        assert_eq!(error["code"], "unsupported_version");
        assert_eq!(control.calls.load(Ordering::SeqCst), 0);

        let ping_id = RequestId::new();
        let ping = ClientMessage::Ping {
            request_id: ping_id,
            nonce: "still-open".to_owned(),
        };
        handle_text(
            &outbound,
            &state,
            &principal,
            &mut watches,
            &mut event_watch,
            ProtocolVersion::V1_0,
            &serde_json::to_string(&ping)?,
        )
        .await
        .map_err(|()| std::io::Error::other("usable session rejected a ping"))?;
        let Some(Message::Text(pong)) = receiver.recv().await else {
            return Err(std::io::Error::other("missing pong after version rejection").into());
        };
        let pong: serde_json::Value = serde_json::from_str(pong.as_str())?;
        assert_eq!(pong["type"], "server.pong");
        assert_eq!(pong["request_id"], ping_id.to_string());
        Ok(())
    }

    #[tokio::test]
    async fn authorization_matrix_denies_before_control_plane_invocation()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let lease_id = xenoteer_protocol::ControlLeaseId::new();
        let command_id = CommandId::new();
        let control = Arc::new(CountingControl::default());
        let state = test_state_with_control(desktop_id, generation, Arc::clone(&control))?;
        let principal = Principal::new("status-only", [Grant::DesktopStatus])?;

        let mut messages = vec![
            submit_message(
                desktop_id,
                generation,
                Command::DesktopProbe(DesktopProbeCommand {}),
            )?,
            submit_message(
                desktop_id,
                generation,
                Command::PointerMove(PointerMoveCommand {
                    target: Point::new(10, 20),
                    duration_ms: Some(10),
                    curve: PointerCurve::Linear,
                }),
            )?,
            submit_message(
                desktop_id,
                generation,
                Command::ApplicationLaunch(ApplicationLaunchCommand {
                    application: ApplicationId::new("xmessage")?,
                    arguments: Vec::new(),
                }),
            )?,
            submit_message(
                desktop_id,
                generation,
                Command::ProcessTerminate(ProcessTerminateCommand {
                    process: test_process_ref(generation),
                    grace_ms: Some(10),
                }),
            )?,
            ClientMessage::CommandWatch {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                command_id,
            },
            ClientMessage::CommandUnwatch {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                command_id,
            },
            ClientMessage::CommandCancel {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                command_id,
            },
            ClientMessage::LeaseGet {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
            },
        ];
        let acquire_id = RequestId::new();
        messages.push(ClientMessage::LeaseAcquire {
            request_id: acquire_id,
            lease: Box::new(LeaseAcquireRequest {
                protocol_version: ProtocolVersion::V1_0,
                request_id: acquire_id,
                desktop_id,
                desktop_generation: generation,
                ttl_ms: Some(1_000),
            }),
        });
        let renew_id = RequestId::new();
        messages.push(ClientMessage::LeaseRenew {
            request_id: renew_id,
            lease: Box::new(LeaseRenewRequest {
                protocol_version: ProtocolVersion::V1_0,
                request_id: renew_id,
                desktop_id,
                desktop_generation: generation,
                lease_id,
                ttl_ms: Some(1_000),
            }),
        });
        let release_id = RequestId::new();
        messages.push(ClientMessage::LeaseRelease {
            request_id: release_id,
            lease: Box::new(LeaseReleaseRequest {
                protocol_version: ProtocolVersion::V1_0,
                request_id: release_id,
                desktop_id,
                desktop_generation: generation,
                lease_id,
            }),
        });
        messages.push(ClientMessage::EventsSubscribe {
            request_id: RequestId::new(),
            desktop_id,
            desktop_generation: generation,
            topics: vec![EventTopic::new("command.lifecycle")?],
            since_sequence: None,
        });
        messages.push(ClientMessage::EventsUnsubscribe {
            request_id: RequestId::new(),
            desktop_id,
            desktop_generation: generation,
        });

        for message in &messages {
            assert_permission_denied_before_control(&state, &principal, &control, message).await?;
        }
        assert_eq!(control.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_accepts_each_command_mutation_grant_and_rejects_non_command_grants()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();

        for grant in [
            Grant::InputControl,
            Grant::ApplicationLaunch,
            Grant::ApplicationTerminate,
            Grant::WindowControl,
            Grant::ClipboardWrite,
            Grant::AccessibilityWrite,
        ] {
            let command_id = CommandId::new();
            let control = Arc::new(CountingControl::default());
            let state = test_state_with_control(desktop_id, generation, Arc::clone(&control))?;
            let principal = Principal::new("command-mutation", [grant])?;
            let message = ClientMessage::CommandCancel {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                command_id,
            };
            let (outbound, mut receiver) = OutboundQueues::bounded(4, 4);
            let mut watches = WatchSet::new();
            let mut event_watch = EventWatch::new();

            handle_text(
                &outbound,
                &state,
                &principal,
                &mut watches,
                &mut event_watch,
                ProtocolVersion::V1_0,
                &serde_json::to_string(&message)?,
            )
            .await
            .map_err(|()| std::io::Error::other("cancellation closed the session"))?;

            let response = receiver
                .recv()
                .await
                .ok_or_else(|| std::io::Error::other("missing cancellation response"))?;
            let Message::Text(response) = response else {
                return Err(std::io::Error::other("cancellation response was not JSON").into());
            };
            let response: serde_json::Value = serde_json::from_str(response.as_str())?;
            assert_eq!(response["code"], "capability_unavailable", "{grant:?}");
            assert_eq!(control.calls.load(Ordering::SeqCst), 1, "{grant:?}");
        }

        for grant in [
            Grant::DesktopStatus,
            Grant::DesktopObserve,
            Grant::ClipboardRead,
            Grant::CaptureRead,
            Grant::ArtifactRead,
            Grant::ArtifactDelete,
            Grant::ViewerRead,
            Grant::AccessibilityRead,
        ] {
            let control = Arc::new(CountingControl::default());
            let state = test_state_with_control(desktop_id, generation, Arc::clone(&control))?;
            let principal = Principal::new("non-command-grant", [grant])?;
            let message = ClientMessage::CommandCancel {
                request_id: RequestId::new(),
                desktop_id,
                desktop_generation: generation,
                command_id: CommandId::new(),
            };

            assert_permission_denied_before_control(&state, &principal, &control, &message).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn hello_resume_without_observe_is_denied_before_control_plane_invocation()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let control = Arc::new(CountingControl::default());
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider = StaticTokenProvider::single(
            TOKEN,
            Principal::new("status-only", [Grant::DesktopStatus])?,
        )?;
        let shared_control: Arc<dyn ControlPlane> = control.clone();
        let application = api_router_with_control(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
            shared_control,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _result = axum::serve(listener, application).await;
        });

        let exercise = async {
            let mut stream = TcpStream::connect(address).await?;
            let authorization = std::str::from_utf8(TOKEN)?;
            let request = format!(
                "GET /v1/ws HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: MDEyMzQ1Njc4OWFiY2RlZg==\r\nAuthorization: Bearer {authorization}\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await?;
            let mut headers = Vec::new();
            while !headers.windows(4).any(|window| window == b"\r\n\r\n") {
                if headers.len() >= 16 * 1_024 {
                    return Err(std::io::Error::other("oversized upgrade response").into());
                }
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).await?;
                headers.push(byte[0]);
            }
            let headers = std::str::from_utf8(&headers)?;
            if !headers.starts_with("HTTP/1.1 101 ") {
                return Err(std::io::Error::other("WebSocket upgrade was not accepted").into());
            }

            let request_id = RequestId::new();
            let hello = ClientHello {
                message_type: "client.hello".to_owned(),
                request_id,
                protocol: xenoteer_protocol::VersionRange::new(1, 0, 0)?,
                client: WebSocketClientDescriptor {
                    name: "authorization-test".to_owned(),
                    version: "1.0.0".to_owned(),
                },
                resume: Some(EventResumeRequest {
                    desktop_id,
                    desktop_generation: generation,
                    event_sequence: 0,
                }),
            };
            write_masked_text(&mut stream, &serde_json::to_vec(&hello)?).await?;
            let (opcode, error) = read_server_frame(&mut stream).await?;
            if opcode != 0x1 {
                return Err(std::io::Error::other("resume denial did not send JSON first").into());
            }
            let error: serde_json::Value = serde_json::from_slice(&error)?;
            assert_eq!(error["type"], "error");
            assert_eq!(error["code"], "permission_denied");
            assert_eq!(error["request_id"], request_id.to_string());
            let (opcode, close) = read_server_frame(&mut stream).await?;
            assert_eq!(opcode, 0x8);
            assert!(close.len() >= 2);
            assert_eq!(u16::from_be_bytes([close[0], close[1]]), 1008);
            Ok::<(), Box<dyn std::error::Error>>(())
        };
        let result = tokio::time::timeout(Duration::from_secs(5), exercise).await;
        server.abort();
        let _server_result = server.await;
        result.map_err(|_| std::io::Error::other("resume authorization test timed out"))??;
        assert_eq!(control.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn replacing_watch_keeps_only_latest_owner() -> Result<(), Box<dyn std::error::Error>> {
        let command_id = CommandId::new();
        let mut watches = WatchSet::new();
        assert!(watches.spawn(command_id, std::future::pending()));
        let first_serial = watches
            .owned
            .get(&command_id)
            .map(|watch| watch.serial)
            .ok_or_else(|| std::io::Error::other("missing first watch"))?;
        assert!(watches.spawn(command_id, std::future::pending()));
        let second_serial = watches
            .owned
            .get(&command_id)
            .map(|watch| watch.serial)
            .ok_or_else(|| std::io::Error::other("missing replacement watch"))?;
        assert_ne!(first_serial, second_serial);
        assert_eq!(watches.owned.len(), 1);
        watches.reap_one().await;
        assert_eq!(
            watches.owned.get(&command_id).map(|watch| watch.serial),
            Some(second_serial)
        );
        watches.shutdown().await;
        assert!(watches.owned.is_empty());
        assert!(!watches.has_tasks());
        Ok(())
    }
}
