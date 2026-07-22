//! Query-free, origin-bound, view-only browser gateway.

use core::fmt;
use std::{
    collections::BTreeMap, future::Future, net::SocketAddr, path::Path as FilePath, pin::Pin,
    sync::Arc, time::Duration,
};

use axum::{
    Extension, Router,
    body::Body,
    extract::{
        Path, State,
        rejection::PathRejection,
        ws::{
            CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade,
            rejection::WebSocketUpgradeRejection,
        },
    },
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_tungstenite::{WebSocketStream, client_async_with_config};
use tungstenite::{
    Message as BackendWebSocketMessage, client::IntoClientRequest, protocol::WebSocketConfig,
};
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, ViewerMode, ViewerOrigin, ViewerTicketAudience,
    ViewerTicketSecret, ViewerTicketUsePolicy,
};

use crate::{
    ApiState,
    viewer::{SharedViewerTicketService, ViewerTicketConsumeAudience, ViewerTicketConsumeRequest},
};

/// Dedicated browser subprotocol prefix carrying the one-time ticket.
pub const VIEWER_TICKET_PROTOCOL_PREFIX: &str = "xenoteer.ticket.";
/// Sole RFB framing protocol selected by the public and loopback gateways.
pub const VIEWER_BINARY_PROTOCOL: &str = "binary";
/// Maximum release-one RFB WebSocket frame or message.
pub const MAX_VIEWER_FRAME_BYTES: usize = 8 * 1_024 * 1_024;
/// Default maximum simultaneous browser viewer sessions.
pub const DEFAULT_MAX_VIEWER_SESSIONS: usize = 16;

const MAX_CONFIGURED_VIEWER_SESSIONS: usize = 64;
const MAX_NO_VNC_ASSET_BYTES: usize = 2 * 1_024 * 1_024;
const MAX_NO_VNC_TREE_BYTES: usize = 8 * 1_024 * 1_024;
const VIEWER_HTML: &[u8] = include_bytes!("../static/viewer/index.html");
const VIEWER_CSS: &[u8] = include_bytes!("../static/viewer/viewer.css");
const VIEWER_MODULE: &[u8] = include_bytes!("../static/viewer/viewer.mjs");
const VIEWER_CSP: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; font-src 'none'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data:; manifest-src 'none'; media-src 'none'; object-src 'none'; script-src 'self'; style-src 'self'";
const CACHE_CONTROL_NO_STORE: &str = "private, no-store";
const SAFE_CLOSE_REASON: &str = "viewer session ended";

const NO_VNC_ASSETS: &[&str] = &[
    "core/base64.js",
    "core/crypto/aes.js",
    "core/crypto/bigint.js",
    "core/crypto/crypto.js",
    "core/crypto/des.js",
    "core/crypto/dh.js",
    "core/crypto/md5.js",
    "core/crypto/rsa.js",
    "core/decoders/copyrect.js",
    "core/decoders/h264.js",
    "core/decoders/hextile.js",
    "core/decoders/jpeg.js",
    "core/decoders/raw.js",
    "core/decoders/rre.js",
    "core/decoders/tight.js",
    "core/decoders/tightpng.js",
    "core/decoders/zlib.js",
    "core/decoders/zrle.js",
    "core/deflator.js",
    "core/display.js",
    "core/encodings.js",
    "core/inflator.js",
    "core/input/domkeytable.js",
    "core/input/fixedkeys.js",
    "core/input/gesturehandler.js",
    "core/input/keyboard.js",
    "core/input/keysym.js",
    "core/input/keysymdef.js",
    "core/input/util.js",
    "core/input/vkeys.js",
    "core/input/xtscancodes.js",
    "core/ra2.js",
    "core/rfb.js",
    "core/util/browser.js",
    "core/util/cursor.js",
    "core/util/element.js",
    "core/util/events.js",
    "core/util/eventtarget.js",
    "core/util/int.js",
    "core/util/logging.js",
    "core/util/strings.js",
    "core/websock.js",
    "vendor/pako/lib/utils/common.js",
    "vendor/pako/lib/zlib/adler32.js",
    "vendor/pako/lib/zlib/crc32.js",
    "vendor/pako/lib/zlib/deflate.js",
    "vendor/pako/lib/zlib/inffast.js",
    "vendor/pako/lib/zlib/inflate.js",
    "vendor/pako/lib/zlib/inftrees.js",
    "vendor/pako/lib/zlib/messages.js",
    "vendor/pako/lib/zlib/trees.js",
    "vendor/pako/lib/zlib/zstream.js",
];

/// Bounded viewer connection timing and capacity policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerGatewayLimits {
    connect_timeout: Duration,
    write_timeout: Duration,
    idle_timeout: Duration,
    session_timeout: Duration,
    maximum_frame_bytes: usize,
    maximum_sessions: usize,
}

impl ViewerGatewayLimits {
    /// Creates a fully bounded viewer gateway policy.
    pub fn new(
        connect_timeout: Duration,
        write_timeout: Duration,
        idle_timeout: Duration,
        session_timeout: Duration,
        maximum_frame_bytes: usize,
        maximum_sessions: usize,
    ) -> Result<Self, ViewerGatewayConfigurationError> {
        if connect_timeout.is_zero()
            || write_timeout.is_zero()
            || idle_timeout.is_zero()
            || session_timeout < idle_timeout
            || connect_timeout > Duration::from_secs(30)
            || write_timeout > Duration::from_secs(30)
            || idle_timeout > Duration::from_secs(60 * 60)
            || session_timeout > Duration::from_secs(24 * 60 * 60)
            || maximum_frame_bytes == 0
            || maximum_frame_bytes > MAX_VIEWER_FRAME_BYTES
            || maximum_sessions == 0
            || maximum_sessions > MAX_CONFIGURED_VIEWER_SESSIONS
        {
            return Err(ViewerGatewayConfigurationError::Limits);
        }
        Ok(Self {
            connect_timeout,
            write_timeout,
            idle_timeout,
            session_timeout,
            maximum_frame_bytes,
            maximum_sessions,
        })
    }

    /// Returns the maximum accepted frame or reassembled message size.
    #[must_use]
    pub const fn maximum_frame_bytes(self) -> usize {
        self.maximum_frame_bytes
    }

    /// Returns the maximum simultaneous viewer sessions.
    #[must_use]
    pub const fn maximum_sessions(self) -> usize {
        self.maximum_sessions
    }
}

impl Default for ViewerGatewayLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            write_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(2 * 60),
            session_timeout: Duration::from_secs(8 * 60 * 60),
            maximum_frame_bytes: MAX_VIEWER_FRAME_BYTES,
            maximum_sessions: DEFAULT_MAX_VIEWER_SESSIONS,
        }
    }
}

/// Safe viewer gateway setup failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ViewerGatewayConfigurationError {
    /// Timing, size, or capacity policy is outside hard limits.
    #[error("viewer gateway limits are invalid")]
    Limits,
    /// The configured backend is not a nonzero loopback socket.
    #[error("viewer backend endpoint is invalid")]
    Backend,
    /// A required pinned noVNC module is missing, unsafe, or oversized.
    #[error("viewer static assets are invalid")]
    Assets,
}

/// Safe, content-free backend failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ViewerBackendError {
    /// The fixed loopback endpoint could not be reached or negotiated.
    #[error("viewer backend is unavailable")]
    Unavailable,
    /// The backend violated the bounded binary WebSocket contract.
    #[error("viewer backend protocol failed")]
    Protocol,
    /// A bounded backend operation timed out.
    #[error("viewer backend operation timed out")]
    Timeout,
}

/// One content-redacted message exchanged with a viewer backend.
#[derive(Clone, PartialEq, Eq)]
pub enum ViewerBackendMessage {
    /// RFB bytes carried in one binary WebSocket message.
    Binary(Bytes),
    /// WebSocket ping payload.
    Ping(Bytes),
    /// WebSocket pong payload.
    Pong(Bytes),
    /// Backend close without forwarding attacker-controlled close details.
    Close,
}

impl fmt::Debug for ViewerBackendMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary(payload) => formatter
                .debug_tuple("Binary")
                .field(&format_args!("{} bytes", payload.len()))
                .finish(),
            Self::Ping(payload) => formatter
                .debug_tuple("Ping")
                .field(&format_args!("{} bytes", payload.len()))
                .finish(),
            Self::Pong(payload) => formatter
                .debug_tuple("Pong")
                .field(&format_args!("{} bytes", payload.len()))
                .finish(),
            Self::Close => formatter.write_str("Close"),
        }
    }
}

/// Boxed future used by the replaceable viewer backend seams.
pub type ViewerBackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One established, binary-only viewer backend WebSocket.
pub trait ViewerBackendConnection: Send + 'static {
    /// Receives one bounded backend message.
    fn receive<'a>(
        &'a mut self,
    ) -> ViewerBackendFuture<'a, Result<Option<ViewerBackendMessage>, ViewerBackendError>>;

    /// Sends one bounded message to the backend.
    fn send<'a>(
        &'a mut self,
        message: ViewerBackendMessage,
    ) -> ViewerBackendFuture<'a, Result<(), ViewerBackendError>>;

    /// Starts a clean backend close without a secret-bearing reason.
    fn close<'a>(&'a mut self) -> ViewerBackendFuture<'a, Result<(), ViewerBackendError>>;
}

/// Replaceable connector that can only return a pre-negotiated backend WebSocket.
pub trait ViewerBackendConnector: Send + Sync + 'static {
    /// Connects and negotiates the configured backend.
    fn connect<'a>(
        &'a self,
        limits: ViewerGatewayLimits,
    ) -> ViewerBackendFuture<'a, Result<Box<dyn ViewerBackendConnection>, ViewerBackendError>>;
}

/// Fixed-path connector for the selected loopback websockify adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackWebsockifyConnector {
    address: SocketAddr,
}

impl LoopbackWebsockifyConnector {
    /// Creates a connector only for an explicit nonzero loopback socket.
    pub fn new(address: SocketAddr) -> Result<Self, ViewerGatewayConfigurationError> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(ViewerGatewayConfigurationError::Backend);
        }
        Ok(Self { address })
    }

    /// Returns the configured loopback socket without a hostname or path choice.
    #[must_use]
    pub const fn address(self) -> SocketAddr {
        self.address
    }
}

impl ViewerBackendConnector for LoopbackWebsockifyConnector {
    fn connect<'a>(
        &'a self,
        limits: ViewerGatewayLimits,
    ) -> ViewerBackendFuture<'a, Result<Box<dyn ViewerBackendConnection>, ViewerBackendError>> {
        Box::pin(async move {
            let stream = TcpStream::connect(self.address)
                .await
                .map_err(|_| ViewerBackendError::Unavailable)?;
            stream
                .set_nodelay(true)
                .map_err(|_| ViewerBackendError::Unavailable)?;
            let mut request = format!("ws://{}/websockify", self.address)
                .into_client_request()
                .map_err(|_| ViewerBackendError::Protocol)?;
            request.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(VIEWER_BINARY_PROTOCOL),
            );
            let config = WebSocketConfig::default()
                .write_buffer_size(0)
                .max_write_buffer_size(limits.maximum_frame_bytes.saturating_add(64 * 1_024))
                .max_message_size(Some(limits.maximum_frame_bytes))
                .max_frame_size(Some(limits.maximum_frame_bytes));
            let (socket, response) = client_async_with_config(request, stream, Some(config))
                .await
                .map_err(|_| ViewerBackendError::Unavailable)?;
            let mut protocols = response
                .headers()
                .get_all(header::SEC_WEBSOCKET_PROTOCOL)
                .iter();
            if protocols.next().map(HeaderValue::as_bytes)
                != Some(VIEWER_BINARY_PROTOCOL.as_bytes())
                || protocols.next().is_some()
            {
                return Err(ViewerBackendError::Protocol);
            }
            Ok(Box::new(WebsockifyConnection { socket }) as Box<dyn ViewerBackendConnection>)
        })
    }
}

struct WebsockifyConnection {
    socket: WebSocketStream<TcpStream>,
}

impl ViewerBackendConnection for WebsockifyConnection {
    fn receive<'a>(
        &'a mut self,
    ) -> ViewerBackendFuture<'a, Result<Option<ViewerBackendMessage>, ViewerBackendError>> {
        Box::pin(async move {
            match self.socket.next().await {
                Some(Ok(BackendWebSocketMessage::Binary(payload))) => {
                    Ok(Some(ViewerBackendMessage::Binary(payload)))
                }
                Some(Ok(BackendWebSocketMessage::Ping(payload))) => {
                    Ok(Some(ViewerBackendMessage::Ping(payload)))
                }
                Some(Ok(BackendWebSocketMessage::Pong(payload))) => {
                    Ok(Some(ViewerBackendMessage::Pong(payload)))
                }
                Some(Ok(BackendWebSocketMessage::Close(_))) | None => {
                    Ok(Some(ViewerBackendMessage::Close))
                }
                Some(Ok(BackendWebSocketMessage::Text(_) | BackendWebSocketMessage::Frame(_))) => {
                    Err(ViewerBackendError::Protocol)
                }
                Some(Err(_)) => Err(ViewerBackendError::Protocol),
            }
        })
    }

    fn send<'a>(
        &'a mut self,
        message: ViewerBackendMessage,
    ) -> ViewerBackendFuture<'a, Result<(), ViewerBackendError>> {
        Box::pin(async move {
            let message = match message {
                ViewerBackendMessage::Binary(payload) => BackendWebSocketMessage::Binary(payload),
                ViewerBackendMessage::Ping(payload) => BackendWebSocketMessage::Ping(payload),
                ViewerBackendMessage::Pong(payload) => BackendWebSocketMessage::Pong(payload),
                ViewerBackendMessage::Close => BackendWebSocketMessage::Close(None),
            };
            self.socket
                .send(message)
                .await
                .map_err(|_| ViewerBackendError::Protocol)
        })
    }

    fn close<'a>(&'a mut self) -> ViewerBackendFuture<'a, Result<(), ViewerBackendError>> {
        Box::pin(async move {
            self.socket
                .close(None)
                .await
                .map_err(|_| ViewerBackendError::Protocol)
        })
    }
}

struct ViewerStaticAssets {
    modules: BTreeMap<&'static str, Bytes>,
}

impl ViewerStaticAssets {
    fn load(root: &FilePath) -> Result<Self, ViewerGatewayConfigurationError> {
        let root_metadata =
            std::fs::symlink_metadata(root).map_err(|_| ViewerGatewayConfigurationError::Assets)?;
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Err(ViewerGatewayConfigurationError::Assets);
        }
        let mut modules = BTreeMap::new();
        let mut total_bytes = 0_usize;
        for &asset in NO_VNC_ASSETS {
            let path = root.join(asset);
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|_| ViewerGatewayConfigurationError::Assets)?;
            let length = usize::try_from(metadata.len())
                .map_err(|_| ViewerGatewayConfigurationError::Assets)?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || length == 0
                || length > MAX_NO_VNC_ASSET_BYTES
            {
                return Err(ViewerGatewayConfigurationError::Assets);
            }
            total_bytes = total_bytes
                .checked_add(length)
                .ok_or(ViewerGatewayConfigurationError::Assets)?;
            if total_bytes > MAX_NO_VNC_TREE_BYTES {
                return Err(ViewerGatewayConfigurationError::Assets);
            }
            let contents =
                std::fs::read(path).map_err(|_| ViewerGatewayConfigurationError::Assets)?;
            if contents.len() != length {
                return Err(ViewerGatewayConfigurationError::Assets);
            }
            modules.insert(asset, Bytes::from(contents));
        }
        Ok(Self { modules })
    }
}

/// Configured public gateway, backend connector, static modules, and session permits.
pub struct ViewerGateway {
    connector: Arc<dyn ViewerBackendConnector>,
    assets: ViewerStaticAssets,
    limits: ViewerGatewayLimits,
    sessions: Arc<Semaphore>,
}

impl ViewerGateway {
    /// Loads the fixed noVNC module closure and enables a configured connector.
    pub fn new(
        connector: Arc<dyn ViewerBackendConnector>,
        no_vnc_root: impl AsRef<FilePath>,
        limits: ViewerGatewayLimits,
    ) -> Result<Self, ViewerGatewayConfigurationError> {
        let limits = ViewerGatewayLimits::new(
            limits.connect_timeout,
            limits.write_timeout,
            limits.idle_timeout,
            limits.session_timeout,
            limits.maximum_frame_bytes,
            limits.maximum_sessions,
        )?;
        Ok(Self {
            connector,
            assets: ViewerStaticAssets::load(no_vnc_root.as_ref())?,
            limits,
            sessions: Arc::new(Semaphore::new(limits.maximum_sessions)),
        })
    }
}

impl fmt::Debug for ViewerGateway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewerGateway")
            .field("limits", &self.limits)
            .field("asset_count", &self.assets.modules.len())
            .field(
                "available_session_permits",
                &self.sessions.available_permits(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ViewerGatewayState {
    tickets: SharedViewerTicketService,
    gateway: Option<Arc<ViewerGateway>>,
}

pub(crate) fn routes(
    tickets: SharedViewerTicketService,
    gateway: Option<Arc<ViewerGateway>>,
) -> Router<ApiState> {
    Router::new()
        .route(
            "/viewer/{desktop_id}/{desktop_generation}/",
            get(viewer_page),
        )
        .route("/viewer/assets/viewer.css", get(viewer_css))
        .route("/viewer/assets/viewer.mjs", get(viewer_module))
        .route("/viewer/vendor/{*asset}", get(viewer_vendor_asset))
        .route(
            "/v1/desktops/{desktop_id}/generations/{desktop_generation}/viewer/ws",
            get(upgrade),
        )
        .layer(Extension(ViewerGatewayState { tickets, gateway }))
}

async fn viewer_page(
    State(state): State<ApiState>,
    Extension(gateway): Extension<ViewerGatewayState>,
    uri: Uri,
    path: Result<Path<(DesktopId, DesktopGeneration)>, PathRejection>,
) -> Response {
    let Ok(Path((desktop_id, generation))) = path else {
        return gateway_rejection(StatusCode::NOT_FOUND);
    };
    if uri.query().is_some()
        || gateway.gateway.is_none()
        || !current_viewer_route(&state, desktop_id, generation)
    {
        return gateway_rejection(StatusCode::NOT_FOUND);
    }
    static_response(VIEWER_HTML, "text/html; charset=utf-8")
}

async fn viewer_css() -> Response {
    static_response(VIEWER_CSS, "text/css; charset=utf-8")
}

async fn viewer_module() -> Response {
    static_response(VIEWER_MODULE, "text/javascript; charset=utf-8")
}

async fn viewer_vendor_asset(
    Extension(state): Extension<ViewerGatewayState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let (Some(gateway), Ok(Path(asset))) = (state.gateway, path) else {
        return gateway_rejection(StatusCode::NOT_FOUND);
    };
    let Some(contents) = gateway.assets.modules.get(asset.as_str()) else {
        return gateway_rejection(StatusCode::NOT_FOUND);
    };
    static_response(contents.clone(), "text/javascript; charset=utf-8")
}

async fn upgrade(
    State(state): State<ApiState>,
    Extension(gateway_state): Extension<ViewerGatewayState>,
    path: Result<Path<(DesktopId, DesktopGeneration)>, PathRejection>,
    headers: HeaderMap,
    websocket: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some(gateway) = gateway_state.gateway else {
        return gateway_rejection(StatusCode::SERVICE_UNAVAILABLE);
    };
    let (Ok(Path((desktop_id, desktop_generation))), Some(origin), Ok(ticket), Ok(websocket)) = (
        path,
        required_origin(&state, &headers),
        parse_viewer_protocols(&headers),
        websocket,
    ) else {
        return gateway_rejection(StatusCode::FORBIDDEN);
    };
    if !current_viewer_route(&state, desktop_id, desktop_generation) {
        return gateway_rejection(StatusCode::FORBIDDEN);
    }
    let Ok(viewer_permit) = Arc::clone(&gateway.sessions).try_acquire_owned() else {
        return gateway_rejection(StatusCode::TOO_MANY_REQUESTS);
    };
    let Some(global_permit) = state.abuse.try_acquire_websocket() else {
        return gateway_rejection(StatusCode::TOO_MANY_REQUESTS);
    };
    let consume = ViewerTicketConsumeRequest {
        ticket,
        audience: ViewerTicketConsumeAudience::ViewerWebsocket,
        desktop_id,
        desktop_generation,
        origin: origin.clone(),
        mode: ViewerMode::ViewOnly,
    };
    let claims = match gateway_state.tickets.consume_for_gateway(consume).await {
        Ok(claims) => claims,
        Err(_) => return gateway_rejection(StatusCode::FORBIDDEN),
    };
    if claims.audience != ViewerTicketAudience::ViewerWebsocket
        || claims.desktop_id != desktop_id
        || claims.desktop_generation != desktop_generation
        || claims.origin != origin
        || claims.mode != ViewerMode::ViewOnly
        || claims.use_policy != ViewerTicketUsePolicy::SingleUse
    {
        return gateway_rejection(StatusCode::FORBIDDEN);
    }
    let backend = match tokio::time::timeout(
        gateway.limits.connect_timeout,
        gateway.connector.connect(gateway.limits),
    )
    .await
    {
        Ok(Ok(backend)) => backend,
        Ok(Err(_)) | Err(_) => return gateway_rejection(StatusCode::SERVICE_UNAVAILABLE),
    };

    websocket
        .protocols([VIEWER_BINARY_PROTOCOL])
        .max_message_size(gateway.limits.maximum_frame_bytes)
        .max_frame_size(gateway.limits.maximum_frame_bytes)
        .write_buffer_size(0)
        .max_write_buffer_size(
            gateway
                .limits
                .maximum_frame_bytes
                .saturating_add(64 * 1_024),
        )
        .on_failed_upgrade(|_| tracing::debug!("viewer WebSocket upgrade failed"))
        .on_upgrade(move |socket| {
            bridge_session(
                socket,
                backend,
                gateway.limits,
                viewer_permit,
                global_permit,
            )
        })
}

fn current_viewer_route(
    state: &ApiState,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> bool {
    if desktop_id != state.desktop_id {
        return false;
    }
    let readiness = state.readiness.snapshot();
    readiness.is_ready() && readiness.desktop_generation == Some(desktop_generation)
}

fn required_origin(state: &ApiState, headers: &HeaderMap) -> Option<ViewerOrigin> {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin = origins.next()?;
    if origins.next().is_some() {
        return None;
    }
    let origin = origin
        .to_str()
        .ok()
        .and_then(|value| ViewerOrigin::new(value).ok())?;
    state.origins.permits_origin(&origin).then_some(origin)
}

pub(crate) fn parse_viewer_protocols(
    headers: &HeaderMap,
) -> Result<ViewerTicketSecret, ViewerProtocolError> {
    let mut binary_count = 0_usize;
    let mut ticket = None;
    for header_value in headers.get_all(header::SEC_WEBSOCKET_PROTOCOL) {
        let value = header_value
            .to_str()
            .map_err(|_| ViewerProtocolError::Invalid)?;
        for protocol in value.split(',').map(str::trim) {
            if protocol == VIEWER_BINARY_PROTOCOL {
                binary_count = binary_count.saturating_add(1);
                continue;
            }
            let Some(secret) = protocol.strip_prefix(VIEWER_TICKET_PROTOCOL_PREFIX) else {
                return Err(ViewerProtocolError::Invalid);
            };
            if ticket.is_some() {
                return Err(ViewerProtocolError::Invalid);
            }
            ticket =
                Some(ViewerTicketSecret::new(secret).map_err(|_| ViewerProtocolError::Invalid)?);
        }
    }
    if binary_count != 1 {
        return Err(ViewerProtocolError::Invalid);
    }
    ticket.ok_or(ViewerProtocolError::Invalid)
}

/// Content-free viewer subprotocol rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ViewerProtocolError {
    #[error("viewer WebSocket protocols are invalid")]
    Invalid,
}

enum SessionInput {
    Browser(Option<Result<Message, axum::Error>>),
    Backend(Result<Option<ViewerBackendMessage>, ViewerBackendError>),
    Idle,
    Lifetime,
}

async fn bridge_session(
    socket: WebSocket,
    mut backend: Box<dyn ViewerBackendConnection>,
    limits: ViewerGatewayLimits,
    _viewer_permit: OwnedSemaphorePermit,
    _global_permit: OwnedSemaphorePermit,
) {
    let (mut browser_sender, mut browser_receiver) = socket.split();
    let lifetime = tokio::time::sleep(limits.session_timeout);
    tokio::pin!(lifetime);
    loop {
        let idle = tokio::time::sleep(limits.idle_timeout);
        tokio::pin!(idle);
        let input = tokio::select! {
            message = browser_receiver.next() => SessionInput::Browser(message),
            message = backend.receive() => SessionInput::Backend(message),
            () = &mut idle => SessionInput::Idle,
            () = &mut lifetime => SessionInput::Lifetime,
        };
        let keep_open = match input {
            SessionInput::Browser(Some(Ok(Message::Binary(payload)))) => {
                if payload.len() > limits.maximum_frame_bytes {
                    false
                } else {
                    timed_backend_send(
                        backend.as_mut(),
                        ViewerBackendMessage::Binary(payload),
                        limits.write_timeout,
                    )
                    .await
                    .is_ok()
                }
            }
            SessionInput::Browser(Some(Ok(Message::Ping(payload)))) => timed_browser_send(
                &mut browser_sender,
                Message::Pong(payload),
                limits.write_timeout,
            )
            .await
            .is_ok(),
            SessionInput::Browser(Some(Ok(Message::Pong(_)))) => true,
            SessionInput::Browser(Some(Ok(Message::Close(_))))
            | SessionInput::Browser(Some(Ok(Message::Text(_))))
            | SessionInput::Browser(None | Some(Err(_)))
            | SessionInput::Backend(Ok(None | Some(ViewerBackendMessage::Close)))
            | SessionInput::Backend(Err(_))
            | SessionInput::Idle
            | SessionInput::Lifetime => false,
            SessionInput::Backend(Ok(Some(ViewerBackendMessage::Binary(payload)))) => {
                if payload.len() > limits.maximum_frame_bytes {
                    false
                } else {
                    timed_browser_send(
                        &mut browser_sender,
                        Message::Binary(payload),
                        limits.write_timeout,
                    )
                    .await
                    .is_ok()
                }
            }
            SessionInput::Backend(Ok(Some(ViewerBackendMessage::Ping(payload)))) => {
                timed_backend_send(
                    backend.as_mut(),
                    ViewerBackendMessage::Pong(payload),
                    limits.write_timeout,
                )
                .await
                .is_ok()
            }
            SessionInput::Backend(Ok(Some(ViewerBackendMessage::Pong(_)))) => true,
        };
        if !keep_open {
            break;
        }
    }
    let _ignored = tokio::time::timeout(limits.write_timeout, backend.close()).await;
    let _ignored =
        tokio::time::timeout(limits.write_timeout, browser_sender.send(viewer_close())).await;
}

async fn timed_backend_send(
    backend: &mut dyn ViewerBackendConnection,
    message: ViewerBackendMessage,
    timeout: Duration,
) -> Result<(), ViewerBackendError> {
    tokio::time::timeout(timeout, backend.send(message))
        .await
        .map_err(|_| ViewerBackendError::Timeout)?
}

async fn timed_browser_send(
    browser: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
    timeout: Duration,
) -> Result<(), ViewerBackendError> {
    tokio::time::timeout(timeout, browser.send(message))
        .await
        .map_err(|_| ViewerBackendError::Timeout)?
        .map_err(|_| ViewerBackendError::Protocol)
}

fn viewer_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: 1000,
        reason: Utf8Bytes::from_static(SAFE_CLOSE_REASON),
    }))
}

fn static_response(body: impl Into<Body>, content_type: &'static str) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    harden_viewer_response(&mut response);
    response
}

fn gateway_rejection(status: StatusCode) -> Response {
    let mut response = status.into_response();
    harden_viewer_response(&mut response);
    response
}

fn harden_viewer_response(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_NO_STORE),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(VIEWER_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "camera=(), display-capture=(), geolocation=(), microphone=(), payment=(), usb=()",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
}

#[cfg(test)]
pub(crate) const fn required_no_vnc_assets() -> &'static [&'static str] {
    NO_VNC_ASSETS
}
