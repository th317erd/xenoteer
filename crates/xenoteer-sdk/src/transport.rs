//! Bounded, retry-neutral HTTP transport for the v1 control API.

use std::{
    fmt, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri,
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE},
};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client as HyperClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use xenoteer_protocol::{
    ArtifactContentType, ArtifactRef, CapabilityReport, CommandEnvelope, CommandId, CommandResult,
    DesktopGeneration, DesktopId, LeaseAcquireRequest, LeaseReleaseRequest, LeaseRenewRequest,
    LeaseStateView, Problem, Sha256Digest, StatusResponse, VersionRange,
};

/// Maximum response-body size accepted by the SDK.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Maximum response body for accessibility endpoints that permit large trees.
pub const MAX_ACCESSIBILITY_RESPONSE_BYTES: usize =
    xenoteer_protocol::MAX_ACCESSIBILITY_SNAPSHOT_BYTES as usize;

/// Maximum long-poll duration accepted by [`Client::wait_command`].
pub const MAX_WAIT_TIMEOUT_MS: u32 = 30_000;

/// Default end-to-end deadline for one HTTP exchange, including response body.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
/// Maximum time client shutdown waits for owned event supervisors.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

const MIN_BEARER_TOKEN_BYTES: usize = 32;
const MAX_BEARER_TOKEN_BYTES: usize = 1024;
const ARTIFACT_SHA256_HEADER: &str = "x-content-sha256";

type RequestBody = UnsyncBoxBody<Bytes, io::Error>;
type HttpClient = HyperClient<HttpsConnector<HttpConnector>, RequestBody>;

/// An origin-only HTTPS or loopback HTTP base URI.
#[derive(Clone, PartialEq, Eq)]
pub struct BaseUri {
    origin: String,
}

impl BaseUri {
    /// Parses an HTTPS origin or `http://loopback-IP[:port]` development origin.
    ///
    /// Paths, queries, fragments, user information, and other schemes are
    /// rejected so endpoint construction cannot silently change API routing.
    pub fn parse(value: &str) -> Result<Self, SdkError> {
        if value.contains('#') {
            return Err(SdkError::InvalidBaseUri);
        }
        let uri = value.parse::<Uri>().map_err(|_| SdkError::InvalidBaseUri)?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none()
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
            || !matches!(uri.path(), "" | "/")
            || uri.query().is_some()
        {
            return Err(SdkError::InvalidBaseUri);
        }

        let authority = uri.authority().ok_or(SdkError::InvalidBaseUri)?;
        let host = uri.host().ok_or(SdkError::InvalidBaseUri)?;
        if uri.scheme_str() == Some("http") && !is_loopback_host(host) {
            return Err(SdkError::InvalidBaseUri);
        }
        Ok(Self {
            origin: format!(
                "{}://{authority}",
                uri.scheme_str().ok_or(SdkError::InvalidBaseUri)?
            ),
        })
    }

    fn endpoint(&self, path_and_query: &str) -> Result<Uri, SdkError> {
        format!("{}{path_and_query}", self.origin)
            .parse()
            .map_err(|_| SdkError::BuildRequest)
    }

    pub(crate) fn websocket_url(&self) -> String {
        if let Some(authority) = self.origin.strip_prefix("https://") {
            format!("wss://{authority}/v1/ws")
        } else {
            format!("ws://{}/v1/ws", self.origin.trim_start_matches("http://"))
        }
    }
}

impl fmt::Debug for BaseUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BaseUri")
            .field("origin", &self.origin)
            .finish()
    }
}

/// A validated bearer credential whose debug representation is always redacted.
#[derive(Clone)]
pub struct BearerToken {
    authorization: HeaderValue,
}

impl BearerToken {
    /// Validates and stores an opaque bearer token.
    pub fn new(value: impl AsRef<[u8]>) -> Result<Self, SdkError> {
        let value = value.as_ref();
        if !(MIN_BEARER_TOKEN_BYTES..=MAX_BEARER_TOKEN_BYTES).contains(&value.len())
            || !is_bearer_token(value)
        {
            return Err(SdkError::InvalidBearerToken);
        }

        let mut authorization = Vec::with_capacity("Bearer ".len() + value.len());
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(value);
        let mut authorization =
            HeaderValue::from_bytes(&authorization).map_err(|_| SdkError::InvalidBearerToken)?;
        authorization.set_sensitive(true);
        Ok(Self { authorization })
    }
}

fn is_loopback_host(host: &str) -> bool {
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken(<redacted>)")
    }
}

/// Stable SDK transport and response error categories.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SdkError {
    /// The base URI is not a supported origin-only URI.
    #[error("invalid SDK base URI")]
    InvalidBaseUri,
    /// Platform TLS roots or the Rustls connector could not be initialized.
    #[error("failed to initialize SDK TLS configuration")]
    TlsConfiguration,
    /// The bearer token is outside its bounds or contains invalid bytes.
    #[error("invalid bearer token")]
    InvalidBearerToken,
    /// A typed request failed protocol validation.
    #[error("invalid SDK request")]
    InvalidRequest,
    /// A request body exceeded the SDK body bound.
    #[error("request body exceeds the {limit}-byte SDK limit")]
    RequestTooLarge {
        /// Configured body-size limit.
        limit: usize,
    },
    /// A typed request could not be encoded.
    #[error("failed to encode SDK request")]
    EncodeRequest,
    /// The HTTP request could not be constructed.
    #[error("failed to build SDK request")]
    BuildRequest,
    /// The request failed before a valid HTTP response was obtained.
    #[error("SDK transport failed")]
    Transport,
    /// The complete HTTP exchange exceeded the configured SDK deadline.
    #[error("SDK request timed out")]
    RequestTimeout,
    /// A response exceeded the SDK body bound.
    #[error("response body exceeds the {limit}-byte SDK limit")]
    ResponseTooLarge {
        /// Configured body-size limit.
        limit: usize,
    },
    /// A successful response did not contain JSON.
    #[error("unexpected response content type")]
    UnexpectedContentType,
    /// A non-success response was not an RFC 9457 problem document.
    #[error("server returned unexpected HTTP status {status}")]
    UnexpectedHttpStatus {
        /// HTTP status code returned by the server.
        status: u16,
    },
    /// A response body or its protocol invariants were invalid.
    #[error("invalid SDK response")]
    InvalidResponse,
    /// An immutable generation- or identity-bound handle is no longer current.
    #[error("SDK handle is stale and must be explicitly reacquired")]
    StaleReference,
    /// The desktop lifetime changed; reacquire generation-bound state before retrying.
    #[error("desktop generation changed; generation-bound state must be reacquired")]
    GenerationChanged,
    /// The server returned a validated RFC 9457 problem document.
    #[error("server returned a structured API problem")]
    Problem(Box<Problem>),
    /// The client and server do not share a supported protocol version.
    #[error("client and server do not share a protocol version")]
    UnsupportedProtocol,
    /// Status does not yet advertise a live desktop generation.
    #[error("desktop session is not currently available")]
    DesktopUnavailable,
    /// A local overall command-wait deadline elapsed without cancelling the command.
    #[error("local command wait timed out; the server command was not cancelled")]
    CommandWaitTimeout,
    /// A released lease handle cannot submit or renew work.
    #[error("controller lease was already released")]
    LeaseReleased,
    /// A scoped lease renewal failed; no further controlled work may be prepared.
    #[error("scoped controller lease renewal failed")]
    ControlLeaseRenewalFailed,
    /// The WebSocket upgrade was permanently rejected.
    #[error("event WebSocket handshake was rejected with HTTP status {status}")]
    EventHandshakeRejected {
        /// Upgrade response status.
        status: u16,
    },
    /// The server rejected the event subscription with a stable protocol code.
    #[error("server rejected the event subscription ({code:?})")]
    EventRejected {
        /// Stable server code.
        code: xenoteer_protocol::ErrorCode,
        /// Bounded server-safe detail.
        detail: String,
    },
    /// The shared client was explicitly closed.
    #[error("SDK client is closed")]
    ClientClosed,
    /// A caller-provided artifact destination rejected output bytes.
    #[error("artifact destination write failed")]
    ArtifactOutput,
}

impl SdkError {
    /// Returns structured server problem details when available.
    #[must_use]
    pub fn problem(&self) -> Option<&Problem> {
        match self {
            Self::Problem(problem) => Some(problem.as_ref()),
            _ => None,
        }
    }
}

/// HTTP client for version-one lease and command endpoints.
#[derive(Clone)]
pub struct Client {
    base: BaseUri,
    token: BearerToken,
    http: HttpClient,
    request_timeout: Duration,
    state: Arc<ClientState>,
}

struct ClientState {
    closed: AtomicBool,
    cancellation: CancellationToken,
    active_event_tasks: AtomicUsize,
    event_tasks_idle: Notify,
}

pub(crate) struct EventTaskGuard {
    state: Arc<ClientState>,
}

impl Drop for EventTaskGuard {
    fn drop(&mut self) {
        if self.state.active_event_tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.state.event_tasks_idle.notify_waiters();
        }
    }
}

impl Client {
    /// Creates a client with automatic replay disabled.
    pub fn new(
        base_uri: impl AsRef<str>,
        bearer_token: impl AsRef<[u8]>,
    ) -> Result<Self, SdkError> {
        let base = BaseUri::parse(base_uri.as_ref())?;
        let token = BearerToken::new(bearer_token)?;
        let connector = HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|_| SdkError::TlsConfiguration)?
            .https_or_http()
            .enable_http1()
            .build();
        let mut builder = HyperClient::builder(TokioExecutor::new());
        builder.retry_canceled_requests(false);
        let http = builder.build::<_, RequestBody>(connector);
        Ok(Self {
            base,
            token,
            http,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            state: Arc::new(ClientState {
                closed: AtomicBool::new(false),
                cancellation: CancellationToken::new(),
                active_event_tasks: AtomicUsize::new(0),
                event_tasks_idle: Notify::new(),
            }),
        })
    }

    /// Replaces the end-to-end exchange deadline with a non-zero bounded value.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self, SdkError> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(SdkError::InvalidRequest);
        }
        self.request_timeout = timeout;
        Ok(self)
    }

    /// Closes this client and every clone derived from it.
    ///
    /// New HTTP and WebSocket operations fail immediately. Owned event
    /// supervisors are cancelled and given a bounded interval to terminate.
    pub async fn close(&self) {
        if !self.state.closed.swap(true, Ordering::AcqRel) {
            self.state.cancellation.cancel();
        }
        let wait = async {
            while self.state.active_event_tasks.load(Ordering::Acquire) != 0 {
                self.state.event_tasks_idle.notified().await;
            }
        };
        let _bounded = tokio::time::timeout(DEFAULT_CLOSE_TIMEOUT, wait).await;
    }

    /// Returns whether this shared transport has been explicitly closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    pub(crate) fn ensure_open(&self) -> Result<(), SdkError> {
        if self.is_closed() {
            Err(SdkError::ClientClosed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.state.cancellation.clone()
    }

    pub(crate) fn register_event_task(&self) -> Result<EventTaskGuard, SdkError> {
        self.ensure_open()?;
        self.state.active_event_tasks.fetch_add(1, Ordering::AcqRel);
        if self.is_closed() {
            let guard = EventTaskGuard {
                state: self.state.clone(),
            };
            drop(guard);
            return Err(SdkError::ClientClosed);
        }
        Ok(EventTaskGuard {
            state: self.state.clone(),
        })
    }

    /// Discovers and validates authenticated server, desktop, and capability status.
    pub async fn status(&self) -> Result<StatusResponse, SdkError> {
        let mut response: StatusResponse = self.send_empty(Method::GET, "/v1/status").await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        let server = VersionRange::new(
            response.protocol_min.major(),
            response.protocol_min.minor(),
            response.protocol_max.minor(),
        )
        .map_err(|_| SdkError::InvalidResponse)?;
        VersionRange::V1
            .negotiate(server)
            .map_err(|_| SdkError::UnsupportedProtocol)?;
        Ok(response)
    }

    /// Reads and validates the current capability report.
    pub async fn capabilities(&self) -> Result<CapabilityReport, SdkError> {
        let mut response: CapabilityReport =
            self.send_empty(Method::GET, "/v1/capabilities").await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        Ok(response)
    }

    /// Reads redacted controller-lease state for a desktop.
    pub async fn lease_state(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<LeaseStateView, SdkError> {
        if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
            return Err(SdkError::InvalidRequest);
        }
        let path = format!("/v1/desktops/{desktop_id}/lease");
        let response = self.send_empty(Method::GET, &path).await?;
        validate_lease_response(response, desktop_id, desktop_generation)
    }

    /// Acquires the exclusive controller lease for a desktop.
    pub async fn acquire_lease(
        &self,
        request: &LeaseAcquireRequest,
    ) -> Result<LeaseStateView, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        let path = format!("/v1/desktops/{}/lease", request.desktop_id);
        let response = self.send_json(Method::POST, &path, request, &[]).await?;
        validate_lease_response(response, request.desktop_id, request.desktop_generation)
    }

    /// Renews an existing controller lease.
    pub async fn renew_lease(
        &self,
        request: &LeaseRenewRequest,
    ) -> Result<LeaseStateView, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        let path = format!(
            "/v1/desktops/{}/lease/{}/renew",
            request.desktop_id, request.lease_id
        );
        let response = self.send_json(Method::POST, &path, request, &[]).await?;
        validate_lease_response(response, request.desktop_id, request.desktop_generation)
    }

    /// Releases an existing controller lease.
    pub async fn release_lease(
        &self,
        request: &LeaseReleaseRequest,
    ) -> Result<LeaseStateView, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        let path = format!(
            "/v1/desktops/{}/lease/{}",
            request.desktop_id, request.lease_id
        );
        let response = self.send_json(Method::DELETE, &path, request, &[]).await?;
        validate_lease_response(response, request.desktop_id, request.desktop_generation)
    }

    /// Submits one command without automatic replay.
    pub async fn submit_command(
        &self,
        command: &CommandEnvelope,
    ) -> Result<CommandResult, SdkError> {
        command.validate().map_err(|_| SdkError::InvalidRequest)?;
        let path = format!("/v1/desktops/{}/commands", command.desktop_id);
        let command_id = command.command_id.to_string();
        let idempotency_key =
            HeaderValue::from_bytes(command_id.as_bytes()).map_err(|_| SdkError::BuildRequest)?;
        let headers = [("idempotency-key", idempotency_key)];
        let response = self
            .send_json(Method::POST, &path, command, &headers)
            .await?;
        validate_command_response(response, command.command_id)
    }

    /// Retrieves the current result for a command.
    pub async fn get_command(
        &self,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> Result<CommandResult, SdkError> {
        validate_command_ids(desktop_id, command_id)?;
        let path = command_path(desktop_id, command_id);
        let response = self.send_empty(Method::GET, &path).await?;
        validate_command_response(response, command_id)
    }

    /// Waits up to `timeout_ms` for a command state change or terminal result.
    pub async fn wait_command(
        &self,
        desktop_id: DesktopId,
        command_id: CommandId,
        timeout_ms: u32,
    ) -> Result<CommandResult, SdkError> {
        validate_command_ids(desktop_id, command_id)?;
        if timeout_ms == 0 || timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return Err(SdkError::InvalidRequest);
        }
        let path = format!(
            "{}/wait?timeout_ms={timeout_ms}",
            command_path(desktop_id, command_id)
        );
        let response = self.send_empty(Method::GET, &path).await?;
        validate_command_response(response, command_id)
    }

    /// Requests cancellation of a command without automatically replaying it.
    pub async fn cancel_command(
        &self,
        desktop_id: DesktopId,
        command_id: CommandId,
    ) -> Result<CommandResult, SdkError> {
        validate_command_ids(desktop_id, command_id)?;
        let path = command_path(desktop_id, command_id);
        let response = self.send_empty(Method::DELETE, &path).await?;
        validate_command_response(response, command_id)
    }

    pub(crate) async fn get_json<R>(&self, path: &str) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
        self.send_empty(Method::GET, path).await
    }

    pub(crate) async fn post_json<T, R>(&self, path: &str, value: &T) -> Result<R, SdkError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.send_json(Method::POST, path, value, &[]).await
    }

    pub(crate) async fn post_json_with_limit<T, R>(
        &self,
        path: &str,
        value: &T,
        response_limit: usize,
    ) -> Result<R, SdkError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.send_json_with_limit(Method::POST, path, value, &[], response_limit)
            .await
    }

    pub(crate) async fn post_json_with_headers<T, R>(
        &self,
        path: &str,
        value: &T,
        headers: &[(&'static str, HeaderValue)],
    ) -> Result<R, SdkError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.send_json(Method::POST, path, value, headers).await
    }

    pub(crate) fn websocket_url(&self) -> String {
        self.base.websocket_url()
    }

    pub(crate) fn authorization_header(&self) -> HeaderValue {
        self.token.authorization.clone()
    }

    pub(crate) async fn upload_artifact(
        &self,
        path: &str,
        content_type: &ArtifactContentType,
        body: Bytes,
    ) -> Result<ArtifactRef, SdkError> {
        self.ensure_open()?;
        if body.is_empty() {
            return Err(SdkError::InvalidRequest);
        }
        if u64::try_from(body.len()).unwrap_or(u64::MAX)
            > xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES
        {
            return Err(SdkError::RequestTooLarge {
                limit: xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES as usize,
            });
        }
        let body_length = body.len();
        let digest = sha256_digest(&body)?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(self.base.endpoint(path)?)
            .header(AUTHORIZATION, self.token.authorization.clone())
            .header(ACCEPT, "application/json, application/problem+json")
            .header(CONTENT_TYPE, content_type.as_str())
            .header(CONTENT_LENGTH, body_length)
            .header(ARTIFACT_SHA256_HEADER, digest.as_str())
            .body(full_body(body))
            .map_err(|_| SdkError::BuildRequest)?;
        let exchange = async {
            let response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            let artifact: ArtifactRef = decode_response(response, MAX_RESPONSE_BYTES).await?;
            artifact.validate().map_err(|_| SdkError::InvalidResponse)?;
            if artifact.content_type != *content_type
                || artifact.content_length != u64::try_from(body_length).unwrap_or(u64::MAX)
                || artifact.sha256 != digest
            {
                return Err(SdkError::InvalidResponse);
            }
            Ok(artifact)
        };
        tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_| SdkError::RequestTimeout)?
    }

    pub(crate) async fn upload_artifact_from<R>(
        &self,
        path: &str,
        content_type: &ArtifactContentType,
        content_length: u64,
        reader: R,
    ) -> Result<ArtifactRef, SdkError>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        self.ensure_open()?;
        if content_length == 0 || content_length > xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES {
            return Err(SdkError::RequestTooLarge {
                limit: xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES as usize,
            });
        }
        let digest_state = Arc::new(Mutex::new((Sha256::new(), 0_u64)));
        let stream_state = digest_state.clone();
        let stream =
            ReaderStream::new(reader.take(content_length.saturating_add(1))).map(move |result| {
                let bytes = result?;
                let mut state = stream_state
                    .lock()
                    .map_err(|_| io::Error::other("artifact digest state failed"))?;
                state.0.update(&bytes);
                state.1 = state
                    .1
                    .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                    .ok_or_else(|| io::Error::other("artifact byte count overflow"))?;
                drop(state);
                Ok::<Frame<Bytes>, io::Error>(Frame::data(bytes))
            });
        let body = StreamBody::new(stream).boxed_unsync();
        let request = Request::builder()
            .method(Method::POST)
            .uri(self.base.endpoint(path)?)
            .header(AUTHORIZATION, self.token.authorization.clone())
            .header(ACCEPT, "application/json, application/problem+json")
            .header(CONTENT_TYPE, content_type.as_str())
            .header(CONTENT_LENGTH, content_length)
            .body(body)
            .map_err(|_| SdkError::BuildRequest)?;
        let exchange = async {
            let response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            let artifact: ArtifactRef = decode_response(response, MAX_RESPONSE_BYTES).await?;
            artifact.validate().map_err(|_| SdkError::InvalidResponse)?;
            let state = digest_state.lock().map_err(|_| SdkError::InvalidResponse)?;
            if state.1 != content_length {
                return Err(SdkError::InvalidResponse);
            }
            let digest = Sha256Digest::new(encode_lower_hex(state.0.clone().finalize().as_ref()))
                .map_err(|_| SdkError::InvalidResponse)?;
            if artifact.content_type != *content_type
                || artifact.content_length != content_length
                || artifact.sha256 != digest
            {
                return Err(SdkError::InvalidResponse);
            }
            Ok(artifact)
        };
        tokio::select! {
            result = tokio::time::timeout(self.request_timeout, exchange) => {
                result.map_err(|_| SdkError::RequestTimeout)?
            }
            () = self.state.cancellation.cancelled() => Err(SdkError::ClientClosed),
        }
    }

    pub(crate) async fn download_artifact_to<W>(
        &self,
        path: &str,
        artifact: &ArtifactRef,
        output: &mut W,
    ) -> Result<(), SdkError>
    where
        W: AsyncWrite + Unpin,
    {
        self.ensure_open()?;
        artifact.validate().map_err(|_| SdkError::InvalidRequest)?;
        let request = Request::builder()
            .method(Method::GET)
            .uri(self.base.endpoint(path)?)
            .header(AUTHORIZATION, self.token.authorization.clone())
            .header(ACCEPT, artifact.content_type.as_str())
            .body(full_body(Bytes::new()))
            .map_err(|_| SdkError::BuildRequest)?;
        let exchange = async {
            let mut response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            if response.status() != StatusCode::OK {
                return Err(decode_error_response(response).await);
            }
            let expected_length =
                usize::try_from(artifact.content_length).map_err(|_| SdkError::InvalidResponse)?;
            validate_exact_header(
                response.headers(),
                CONTENT_LENGTH.as_str(),
                &expected_length.to_string(),
            )?;
            validate_exact_header(
                response.headers(),
                CONTENT_TYPE.as_str(),
                artifact.content_type.as_str(),
            )?;
            validate_exact_header(
                response.headers(),
                ARTIFACT_SHA256_HEADER,
                artifact.sha256.as_str(),
            )?;
            if response.headers().contains_key(CONTENT_RANGE) {
                return Err(SdkError::InvalidResponse);
            }
            let mut received = 0_usize;
            let mut digest = Sha256::new();
            while let Some(frame) = response.body_mut().frame().await {
                let frame = frame.map_err(|_| SdkError::Transport)?;
                if let Ok(data) = frame.into_data() {
                    received =
                        received
                            .checked_add(data.len())
                            .ok_or(SdkError::ResponseTooLarge {
                                limit: expected_length,
                            })?;
                    if received > expected_length {
                        return Err(SdkError::ResponseTooLarge {
                            limit: expected_length,
                        });
                    }
                    digest.update(&data);
                    output
                        .write_all(&data)
                        .await
                        .map_err(|_| SdkError::ArtifactOutput)?;
                }
            }
            if received != expected_length
                || encode_lower_hex(digest.finalize().as_ref()) != artifact.sha256.as_str()
            {
                return Err(SdkError::InvalidResponse);
            }
            output.flush().await.map_err(|_| SdkError::ArtifactOutput)?;
            Ok(())
        };
        tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_| SdkError::RequestTimeout)?
    }

    pub(crate) async fn delete_artifact(&self, path: &str) -> Result<(), SdkError> {
        self.ensure_open()?;
        let request = Request::builder()
            .method(Method::DELETE)
            .uri(self.base.endpoint(path)?)
            .header(AUTHORIZATION, self.token.authorization.clone())
            .header(ACCEPT, "application/problem+json")
            .body(full_body(Bytes::new()))
            .map_err(|_| SdkError::BuildRequest)?;
        let exchange = async {
            let response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            if response.status() != StatusCode::NO_CONTENT {
                return Err(decode_error_response(response).await);
            }
            validate_content_length(response.headers(), 0)?;
            let body = collect_bounded(response.into_body(), 0).await?;
            if !body.is_empty() {
                return Err(SdkError::InvalidResponse);
            }
            Ok(())
        };
        tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_| SdkError::RequestTimeout)?
    }

    async fn send_json<T, R>(
        &self,
        method: Method,
        path: &str,
        value: &T,
        extra_headers: &[(&'static str, HeaderValue)],
    ) -> Result<R, SdkError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.send_json_with_limit(method, path, value, extra_headers, MAX_RESPONSE_BYTES)
            .await
    }

    async fn send_json_with_limit<T, R>(
        &self,
        method: Method,
        path: &str,
        value: &T,
        extra_headers: &[(&'static str, HeaderValue)],
        response_limit: usize,
    ) -> Result<R, SdkError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let body = serde_json::to_vec(value).map_err(|_| SdkError::EncodeRequest)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(SdkError::RequestTooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        self.send(
            method,
            path,
            Bytes::from(body),
            true,
            extra_headers,
            response_limit,
        )
        .await
    }

    async fn send_empty<R>(&self, method: Method, path: &str) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
        self.send_empty_with_limit(method, path, MAX_RESPONSE_BYTES)
            .await
    }

    async fn send_empty_with_limit<R>(
        &self,
        method: Method,
        path: &str,
        response_limit: usize,
    ) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
        self.send(method, path, Bytes::new(), false, &[], response_limit)
            .await
    }

    async fn send<R>(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        has_json_body: bool,
        extra_headers: &[(&'static str, HeaderValue)],
        response_limit: usize,
    ) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
        self.ensure_open()?;
        let mut builder = Request::builder()
            .method(method)
            .uri(self.base.endpoint(path)?)
            .header(AUTHORIZATION, self.token.authorization.clone())
            .header(ACCEPT, "application/json, application/problem+json");
        if has_json_body {
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        for (name, value) in extra_headers {
            builder = builder.header(*name, value.clone());
        }
        let request = builder
            .body(full_body(body))
            .map_err(|_| SdkError::BuildRequest)?;

        // This is intentionally the only network attempt made for the request.
        let exchange = async {
            let response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            decode_response(response, response_limit).await
        };
        tokio::select! {
            result = tokio::time::timeout(self.request_timeout, exchange) => {
                result.map_err(|_| SdkError::RequestTimeout)?
            }
            () = self.state.cancellation.cancelled() => Err(SdkError::ClientClosed),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("base", &self.base)
            .field("token", &self.token)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

fn command_path(desktop_id: DesktopId, command_id: CommandId) -> String {
    format!("/v1/desktops/{desktop_id}/commands/{command_id}")
}

fn full_body(bytes: Bytes) -> RequestBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn validate_command_ids(desktop_id: DesktopId, command_id: CommandId) -> Result<(), SdkError> {
    if desktop_id.as_uuid().is_nil() || command_id.as_uuid().is_nil() {
        return Err(SdkError::InvalidRequest);
    }
    Ok(())
}

fn validate_lease_response(
    response: LeaseStateView,
    expected_desktop_id: DesktopId,
    expected_generation: DesktopGeneration,
) -> Result<LeaseStateView, SdkError> {
    response.validate().map_err(|_| SdkError::InvalidResponse)?;
    if response.desktop_id != expected_desktop_id
        || response.desktop_generation != expected_generation
    {
        return Err(SdkError::InvalidResponse);
    }
    Ok(response)
}

fn validate_command_response(
    response: CommandResult,
    expected_command_id: CommandId,
) -> Result<CommandResult, SdkError> {
    response.validate().map_err(|_| SdkError::InvalidResponse)?;
    if response.command_id() != expected_command_id {
        return Err(SdkError::InvalidResponse);
    }
    Ok(response)
}

async fn decode_response<R>(
    response: Response<Incoming>,
    response_limit: usize,
) -> Result<R, SdkError>
where
    R: DeserializeOwned,
{
    let status = response.status();
    validate_content_length(response.headers(), response_limit)?;
    let content_type = response.headers().get(CONTENT_TYPE).cloned();
    let body = collect_bounded(response.into_body(), response_limit).await?;

    if !status.is_success() {
        if !content_type_is(content_type.as_ref(), "application/problem+json") {
            return Err(SdkError::UnexpectedHttpStatus {
                status: status.as_u16(),
            });
        }
        let problem: Problem =
            serde_json::from_slice(&body).map_err(|_| SdkError::InvalidResponse)?;
        problem.validate().map_err(|_| SdkError::InvalidResponse)?;
        if problem.status() != status.as_u16() {
            return Err(SdkError::InvalidResponse);
        }
        return Err(SdkError::Problem(Box::new(problem)));
    }

    if !content_type_is(content_type.as_ref(), "application/json") {
        return Err(SdkError::UnexpectedContentType);
    }
    serde_json::from_slice(&body).map_err(|_| SdkError::InvalidResponse)
}

async fn decode_error_response(response: Response<Incoming>) -> SdkError {
    match decode_response::<serde_json::Value>(response, MAX_RESPONSE_BYTES).await {
        Err(error) => error,
        Ok(_) => SdkError::InvalidResponse,
    }
}

fn validate_exact_header(
    headers: &HeaderMap,
    name: &'static str,
    expected: &str,
) -> Result<(), SdkError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(SdkError::InvalidResponse)?;
    if values.next().is_some() || value.as_bytes() != expected.as_bytes() {
        return Err(SdkError::InvalidResponse);
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> Result<Sha256Digest, SdkError> {
    Sha256Digest::new(encode_lower_hex(Sha256::digest(bytes).as_ref()))
        .map_err(|_| SdkError::EncodeRequest)
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn validate_content_length(headers: &HeaderMap, limit: usize) -> Result<(), SdkError> {
    let mut lengths = headers.get_all(CONTENT_LENGTH).iter();
    let Some(length) = lengths.next() else {
        return Ok(());
    };
    if lengths.next().is_some() {
        return Err(SdkError::InvalidResponse);
    }
    let length = length
        .to_str()
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(SdkError::InvalidResponse)?;
    if length > limit {
        return Err(SdkError::ResponseTooLarge { limit });
    }
    Ok(())
}

async fn collect_bounded(mut body: Incoming, limit: usize) -> Result<Bytes, SdkError> {
    let mut collected = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| SdkError::Transport)?;
        if let Ok(data) = frame.into_data() {
            let new_len = collected
                .len()
                .checked_add(data.len())
                .ok_or(SdkError::ResponseTooLarge { limit })?;
            if new_len > limit {
                return Err(SdkError::ResponseTooLarge { limit });
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(collected.freeze())
}

fn content_type_is(value: Option<&HeaderValue>, expected: &str) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn is_bearer_token(value: &[u8]) -> bool {
    let unpadded_len = value
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(value.len());
    unpadded_len != 0
        && value[..unpadded_len].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
        && value[unpadded_len..].iter().all(|byte| *byte == b'=')
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, time::Duration};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
        time::timeout,
    };
    use xenoteer_protocol::{
        ArtifactId, ArtifactPurpose, Command, DesktopProbeCommand, EffectStage, ErrorCode,
        ProtocolVersion, RequestId, RetryAdvice, Timestamp,
    };

    use super::*;

    type TestError = Box<dyn Error + Send + Sync>;

    #[test]
    fn credentials_are_redacted_and_base_uri_is_strict() -> Result<(), TestError> {
        let secret = "sdk-secret-canary-0123456789abcdef";
        let token = BearerToken::new(secret)?;
        let client = Client::new("http://127.0.0.1:8080", secret)?;
        assert!(BaseUri::parse("http://[::1]:8080").is_ok());
        assert!(BaseUri::parse("https://xenoteer.example").is_ok());
        assert!(BaseUri::parse("https://xenoteer.example:8443/").is_ok());
        assert_eq!(
            BaseUri::parse("https://xenoteer.example:8443/")?.websocket_url(),
            "wss://xenoteer.example:8443/v1/ws"
        );

        let token_debug = format!("{token:?}");
        let client_debug = format!("{client:?}");
        assert!(token_debug.contains("<redacted>"));
        assert!(!token_debug.contains(secret));
        assert!(!client_debug.contains(secret));
        assert!(token.authorization.is_sensitive());
        assert!(matches!(
            BearerToken::new("invalid=padding=0123456789abcdefgh"),
            Err(SdkError::InvalidBearerToken)
        ));

        for invalid in [
            "http://127.0.0.1/api",
            "http://127.0.0.1/?tenant=one",
            "http://user@127.0.0.1",
            "http://localhost:8080",
            "http://192.0.2.1:8080",
            "http://example.test:8080",
            "ftp://127.0.0.1:8080",
            "https://user@example.test",
            "https://example.test/api",
            "https://example.test?tenant=one",
            "https://example.test/#fragment",
            "127.0.0.1:8080",
        ] {
            assert!(
                matches!(BaseUri::parse(invalid), Err(SdkError::InvalidBaseUri)),
                "unexpectedly accepted {invalid}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn submit_sends_command_id_as_idempotency_key() -> Result<(), TestError> {
        let command = probe_command()?;
        let result = CommandResult::accepted(
            command.command_id,
            Timestamp::parse("2026-07-21T00:00:00Z")?,
        );
        let response = json_response("200 OK", "application/json", &result)?;
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;

        let returned = client.submit_command(&command).await?;
        assert_eq!(returned.command_id(), command.command_id);

        let request = String::from_utf8(server.await??)?;
        let request = request.to_ascii_lowercase();
        assert!(request.starts_with("post /v1/desktops/"));
        assert!(request.contains(&format!("idempotency-key: {}", command.command_id)));
        Ok(())
    }

    #[tokio::test]
    async fn problem_json_is_decoded_and_status_checked() -> Result<(), TestError> {
        let desktop_id = DesktopId::new();
        let command_id = CommandId::new();
        let problem = Problem::new(
            409,
            ErrorCode::LeaseConflict,
            "Lease conflict",
            "A controller lease is required",
            RetryAdvice::AfterResync,
            EffectStage::Accepted,
        )?;
        let response = json_response("409 Conflict", "application/problem+json", &problem)?;
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;

        let error = client
            .get_command(desktop_id, command_id)
            .await
            .err()
            .ok_or("expected a structured problem")?;
        let decoded = error.problem().ok_or("problem details were lost")?;
        assert_eq!(decoded.status(), 409);
        assert_eq!(decoded.code(), ErrorCode::LeaseConflict);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn declared_oversize_response_is_rejected_before_collection() -> Result<(), TestError> {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;

        let result = client.get_command(DesktopId::new(), CommandId::new()).await;
        assert!(matches!(
            result,
            Err(SdkError::ResponseTooLarge {
                limit: MAX_RESPONSE_BYTES
            })
        ));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn streamed_oversize_response_is_rejected_during_collection() -> Result<(), TestError> {
        let body = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;

        let result = client.get_command(DesktopId::new(), CommandId::new()).await;
        assert!(matches!(
            result,
            Err(SdkError::ResponseTooLarge {
                limit: MAX_RESPONSE_BYTES
            })
        ));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn exchange_timeout_bounds_a_server_that_never_replies() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _request = read_request(&mut stream).await?;
            std::future::pending::<io::Result<()>>().await
        });
        let client =
            Client::new(base, test_token())?.with_request_timeout(Duration::from_millis(20))?;

        let result = client.get_command(DesktopId::new(), CommandId::new()).await;
        assert!(matches!(result, Err(SdkError::RequestTimeout)));
        server.abort();
        let _observed_abort = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn close_is_shared_and_rejects_every_derived_transport_call() -> Result<(), TestError> {
        let client = Client::new("http://127.0.0.1:9", test_token())?;
        let derived = client.clone();
        client.close().await;

        assert!(client.is_closed());
        assert!(derived.is_closed());
        assert!(matches!(
            derived
                .get_command(DesktopId::new(), CommandId::new())
                .await,
            Err(SdkError::ClientClosed)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn close_waits_for_owned_event_guard_and_cancels_inflight_http() -> Result<(), TestError>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _request = read_request(&mut stream).await?;
            std::future::pending::<io::Result<()>>().await
        });
        let client = Client::new(base, test_token())?;
        let guard = client.register_event_task()?;
        let request_client = client.clone();
        let request = tokio::spawn(async move {
            request_client
                .get_command(DesktopId::new(), CommandId::new())
                .await
        });
        tokio::task::yield_now().await;
        let close_client = client.clone();
        let close = tokio::spawn(async move {
            close_client.close().await;
        });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());
        drop(guard);
        timeout(Duration::from_secs(1), close).await??;
        assert!(matches!(
            timeout(Duration::from_secs(1), request).await??,
            Err(SdkError::ClientClosed)
        ));
        server.abort();
        let _aborted = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_submission_is_not_replayed() -> Result<(), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let primer_desktop_id = DesktopId::new();
        let primer_command_id = CommandId::new();
        let primer_result =
            CommandResult::accepted(primer_command_id, Timestamp::parse("2026-07-21T00:00:00Z")?);
        let primer_body = serde_json::to_vec(&primer_result)?;
        let primer_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            primer_body.len()
        );
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await?;
            read_request(&mut first).await?;
            first.write_all(primer_response.as_bytes()).await?;
            first.write_all(&primer_body).await?;

            // The command is sent over this now-reused connection. Closing it
            // after reading the request exercises Hyper's canceled-request
            // replay path, which the SDK disables in `Client::new`.
            read_request(&mut first).await?;
            first.shutdown().await?;

            match timeout(Duration::from_millis(500), listener.accept()).await {
                Ok(Ok((_second, _))) => Ok::<usize, io::Error>(2),
                Ok(Err(error)) => Err(error),
                Err(_) => Ok(1),
            }
        });
        let client = Client::new(base, test_token())?;

        let primer = client
            .get_command(primer_desktop_id, primer_command_id)
            .await?;
        assert_eq!(primer.command_id(), primer_command_id);

        let result = client.submit_command(&probe_command()?).await;
        assert!(matches!(result, Err(SdkError::Transport)));
        assert_eq!(server.await??, 1);
        Ok(())
    }

    #[tokio::test]
    async fn endpoint_specific_json_limit_accepts_valid_large_accessibility_shape()
    -> Result<(), TestError> {
        let value = serde_json::json!({"payload": "x".repeat(MAX_RESPONSE_BYTES + 1)});
        let response = json_response("200 OK", "application/json", &value)?;
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let returned: serde_json::Value = client
            .post_json_with_limit(
                "/v1/desktops/test/accessibility/elements/snapshot",
                &serde_json::json!({}),
                MAX_ACCESSIBILITY_RESPONSE_BYTES,
            )
            .await?;
        assert_eq!(returned, value);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn artifact_download_streams_only_after_exact_metadata_and_digest_validation()
    -> Result<(), TestError> {
        let body = Bytes::from_static(b"verified-artifact-body");
        let artifact = artifact_ref(ArtifactPurpose::Screenshot, &body)?;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nx-content-sha256: {}\r\nConnection: close\r\n\r\n",
            artifact.content_type.as_str(),
            artifact.content_length,
            artifact.sha256.as_str()
        )
        .into_bytes();
        let mut response = response;
        response.extend_from_slice(&body);
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let mut output = Vec::new();
        client
            .download_artifact_to("/v1/artifacts/test", &artifact, &mut output)
            .await?;
        assert_eq!(output, body);
        let request = String::from_utf8(server.await??)?;
        assert!(request.starts_with("GET /v1/artifacts/test HTTP/1.1"));
        Ok(())
    }

    #[tokio::test]
    async fn artifact_download_rejects_metadata_before_writing_output() -> Result<(), TestError> {
        let body = Bytes::from_static(b"verified-artifact-body");
        let artifact = artifact_ref(ArtifactPurpose::Screenshot, &body)?;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nx-content-sha256: {}\r\nConnection: close\r\n\r\n",
            artifact.content_length,
            artifact.sha256.as_str()
        )
        .into_bytes();
        let mut response = response;
        response.extend_from_slice(&body);
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let mut output = Vec::new();
        assert!(matches!(
            client
                .download_artifact_to("/v1/artifacts/test", &artifact, &mut output)
                .await,
            Err(SdkError::InvalidResponse)
        ));
        assert!(output.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn artifact_download_rejects_partial_content_without_writing() -> Result<(), TestError> {
        let body = Bytes::from_static(b"partial");
        let complete = Bytes::from_static(b"partial-artifact");
        let artifact = artifact_ref(ArtifactPurpose::Screenshot, &complete)?;
        let mut response = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: {}\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nx-content-sha256: {}\r\nConnection: close\r\n\r\n",
            artifact.content_type.as_str(),
            body.len(),
            body.len() - 1,
            artifact.content_length,
            artifact.sha256.as_str()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let mut output = Vec::new();
        assert!(matches!(
            client
                .download_artifact_to("/v1/artifacts/test", &artifact, &mut output)
                .await,
            Err(SdkError::UnexpectedContentType | SdkError::InvalidResponse)
        ));
        assert!(output.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn artifact_upload_sends_exact_length_type_and_digest() -> Result<(), TestError> {
        let body = Bytes::from_static(b"clipboard artifact");
        let artifact = artifact_ref(ArtifactPurpose::ClipboardInput, &body)?;
        let response = json_response("201 Created", "application/json", &artifact)?;
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let content_type = ArtifactContentType::new("application/octet-stream")?;
        let returned = client
            .upload_artifact(
                "/v1/artifacts?purpose=clipboard_input",
                &content_type,
                body.clone(),
            )
            .await?;
        assert_eq!(returned, artifact);
        let request = String::from_utf8(server.await??)?;
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.starts_with("post /v1/artifacts?purpose=clipboard_input http/1.1"));
        assert!(request_lower.contains("content-type: application/octet-stream"));
        assert!(request_lower.contains(&format!("content-length: {}", body.len())));
        assert!(request_lower.contains(&format!("x-content-sha256: {}", artifact.sha256.as_str())));
        Ok(())
    }

    #[tokio::test]
    async fn artifact_upload_streams_an_exact_async_reader_without_a_body_buffer()
    -> Result<(), TestError> {
        let body = Bytes::from(vec![b'x'; 64 * 1024]);
        let artifact = artifact_ref(ArtifactPurpose::ClipboardInput, &body)?;
        let response = json_response("201 Created", "application/json", &artifact)?;
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        let content_type = ArtifactContentType::new("application/octet-stream")?;
        let (mut writer, reader) = tokio::io::duplex(1_024);
        let payload = body.clone();
        let producer = tokio::spawn(async move {
            for chunk in payload.chunks(509) {
                writer.write_all(chunk).await?;
                tokio::task::yield_now().await;
            }
            writer.shutdown().await
        });
        let returned = client
            .upload_artifact_from(
                "/v1/artifacts?purpose=clipboard_input",
                &content_type,
                u64::try_from(body.len())?,
                reader,
            )
            .await?;
        assert_eq!(returned, artifact);
        producer.await??;
        let request = server.await??;
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or("missing request header terminator")?
            + 4;
        assert_eq!(&request[header_end..], body.as_ref());
        let headers = String::from_utf8(request[..header_end].to_vec())?.to_ascii_lowercase();
        assert!(headers.contains(&format!("content-length: {}", body.len())));
        // The digest is computed while streaming and verified against the
        // returned immutable reference; it need not be known as a request header.
        assert!(!headers.contains("x-content-sha256:"));
        Ok(())
    }

    #[tokio::test]
    async fn artifact_upload_rejects_a_reader_shorter_than_its_declared_length()
    -> Result<(), TestError> {
        let declared = Bytes::from_static(b"declared-eight");
        let artifact = artifact_ref(ArtifactPurpose::ClipboardInput, &declared)?;
        let response = json_response("201 Created", "application/json", &artifact)?;
        let (base, server) = serve_once(response).await?;
        let client =
            Client::new(base, test_token())?.with_request_timeout(Duration::from_millis(150))?;
        let content_type = ArtifactContentType::new("application/octet-stream")?;
        let reader = std::io::Cursor::new(b"short".to_vec());
        assert!(matches!(
            client
                .upload_artifact_from(
                    "/v1/artifacts?purpose=clipboard_input",
                    &content_type,
                    u64::try_from(declared.len())?,
                    reader,
                )
                .await,
            Err(SdkError::Transport | SdkError::RequestTimeout | SdkError::InvalidResponse)
        ));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn artifact_upload_rejects_a_reader_longer_than_its_declared_length()
    -> Result<(), TestError> {
        let declared = Bytes::from_static(b"declared");
        let artifact = artifact_ref(ArtifactPurpose::ClipboardInput, &declared)?;
        let response = json_response("201 Created", "application/json", &artifact)?;
        let (base, server) = serve_once(response).await?;
        let client =
            Client::new(base, test_token())?.with_request_timeout(Duration::from_millis(150))?;
        let content_type = ArtifactContentType::new("application/octet-stream")?;
        let reader = std::io::Cursor::new(b"declared-extra".to_vec());
        assert!(matches!(
            client
                .upload_artifact_from(
                    "/v1/artifacts?purpose=clipboard_input",
                    &content_type,
                    u64::try_from(declared.len())?,
                    reader,
                )
                .await,
            Err(SdkError::Transport | SdkError::RequestTimeout | SdkError::InvalidResponse)
        ));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn artifact_delete_requires_an_exact_empty_no_content_response() -> Result<(), TestError>
    {
        let response =
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (base, server) = serve_once(response).await?;
        let client = Client::new(base, test_token())?;
        client
            .delete_artifact("/v1/artifacts/test?desktop_id=a&desktop_generation=b")
            .await?;
        let request = String::from_utf8(server.await??)?;
        assert!(
            request.starts_with(
                "DELETE /v1/artifacts/test?desktop_id=a&desktop_generation=b HTTP/1.1"
            )
        );
        Ok(())
    }

    fn test_token() -> &'static str {
        "test-token-0123456789abcdefghijklmnop"
    }

    fn probe_command() -> Result<CommandEnvelope, TestError> {
        Ok(CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            DesktopId::new(),
            DesktopGeneration::new(),
            Command::DesktopProbe(DesktopProbeCommand {}),
        )?)
    }

    fn artifact_ref(purpose: ArtifactPurpose, body: &[u8]) -> Result<ArtifactRef, TestError> {
        Ok(ArtifactRef {
            artifact_id: ArtifactId::new(),
            purpose,
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            content_type: ArtifactContentType::new("application/octet-stream")?,
            content_length: u64::try_from(body.len())?,
            sha256: sha256_digest(body)?,
            created_at: Timestamp::parse("2026-07-23T00:00:00Z")?,
            expires_at: Timestamp::parse("2026-07-23T01:00:00Z")?,
        })
    }

    fn json_response<T: Serialize>(
        status: &str,
        content_type: &str,
        value: &T,
    ) -> Result<Vec<u8>, TestError> {
        let body = serde_json::to_vec(value)?;
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        Ok(response)
    }

    async fn serve_once(
        response: Vec<u8>,
    ) -> Result<(String, JoinHandle<io::Result<Vec<u8>>>), TestError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let request = read_request(&mut stream).await?;
            match stream.write_all(&response).await {
                Ok(()) => stream.shutdown().await?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => return Err(error),
            }
            Ok(request)
        });
        Ok((base, server))
    }

    async fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request headers ended early",
                ));
            }
            request.extend_from_slice(&buffer[..read]);
        };

        let header_text = std::str::from_utf8(&request[..header_end]).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "request headers were not UTF-8")
        })?;
        let content_length = header_text
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>())
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid content length"))?
            .unwrap_or_default();
        let complete_length = header_end
            .checked_add(content_length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request length overflow"))?;
        while request.len() < complete_length {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request body ended early",
                ));
            }
            request.extend_from_slice(&buffer[..read]);
        }
        Ok(request)
    }
}
