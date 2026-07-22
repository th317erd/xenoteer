//! Bounded, retry-neutral HTTP transport for the v1 control API.

use std::{fmt, time::Duration};

use bytes::{Bytes, BytesMut};
use http::{
    HeaderMap, HeaderValue, Method, Request, Response, Uri,
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE},
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::{
    client::legacy::{Client as HyperClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use xenoteer_protocol::{
    CommandEnvelope, CommandId, CommandResult, DesktopGeneration, DesktopId, LeaseAcquireRequest,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, Problem,
};

/// Maximum response-body size accepted by the SDK.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum long-poll duration accepted by [`Client::wait_command`].
pub const MAX_WAIT_TIMEOUT_MS: u32 = 30_000;

/// Default end-to-end deadline for one HTTP exchange, including response body.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

const MIN_BEARER_TOKEN_BYTES: usize = 32;
const MAX_BEARER_TOKEN_BYTES: usize = 1024;

type HttpClient = HyperClient<HttpConnector, Full<Bytes>>;

/// An origin-only HTTP base URI.
#[derive(Clone, PartialEq, Eq)]
pub struct BaseUri {
    origin: String,
}

impl BaseUri {
    /// Parses an `http://loopback-IP[:port]` origin.
    ///
    /// TLS is expected to terminate at an external gateway in the initial SDK.
    /// Paths, queries, fragments, user information, and non-HTTP schemes are
    /// rejected so endpoint construction cannot silently change API routing.
    pub fn parse(value: &str) -> Result<Self, SdkError> {
        let uri = value.parse::<Uri>().map_err(|_| SdkError::InvalidBaseUri)?;
        if uri.scheme_str() != Some("http")
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
        if !is_loopback_host(host) {
            return Err(SdkError::InvalidBaseUri);
        }
        Ok(Self {
            origin: format!("http://{authority}"),
        })
    }

    fn endpoint(&self, path_and_query: &str) -> Result<Uri, SdkError> {
        format!("{}{path_and_query}", self.origin)
            .parse()
            .map_err(|_| SdkError::BuildRequest)
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
    /// The base URI is not an origin-only HTTP URI.
    #[error("invalid SDK base URI")]
    InvalidBaseUri,
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
    /// The server returned a validated RFC 9457 problem document.
    #[error("server returned a structured API problem")]
    Problem(Box<Problem>),
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
}

impl Client {
    /// Creates a client with automatic replay disabled.
    pub fn new(
        base_uri: impl AsRef<str>,
        bearer_token: impl AsRef<[u8]>,
    ) -> Result<Self, SdkError> {
        let base = BaseUri::parse(base_uri.as_ref())?;
        let token = BearerToken::new(bearer_token)?;
        let mut builder = HyperClient::builder(TokioExecutor::new());
        builder.retry_canceled_requests(false);
        let http = builder.build_http::<Full<Bytes>>();
        Ok(Self {
            base,
            token,
            http,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
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
        let body = serde_json::to_vec(value).map_err(|_| SdkError::EncodeRequest)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(SdkError::RequestTooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        self.send(method, path, Bytes::from(body), true, extra_headers)
            .await
    }

    async fn send_empty<R>(&self, method: Method, path: &str) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
        self.send(method, path, Bytes::new(), false, &[]).await
    }

    async fn send<R>(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        has_json_body: bool,
        extra_headers: &[(&'static str, HeaderValue)],
    ) -> Result<R, SdkError>
    where
        R: DeserializeOwned,
    {
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
            .body(Full::new(body))
            .map_err(|_| SdkError::BuildRequest)?;

        // This is intentionally the only network attempt made for the request.
        let exchange = async {
            let response = self
                .http
                .request(request)
                .await
                .map_err(|_| SdkError::Transport)?;
            decode_response(response).await
        };
        tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_| SdkError::RequestTimeout)?
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

async fn decode_response<R>(response: Response<Incoming>) -> Result<R, SdkError>
where
    R: DeserializeOwned,
{
    let status = response.status();
    validate_content_length(response.headers())?;
    let content_type = response.headers().get(CONTENT_TYPE).cloned();
    let body = collect_bounded(response.into_body()).await?;

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

fn validate_content_length(headers: &HeaderMap) -> Result<(), SdkError> {
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
    if length > MAX_RESPONSE_BYTES {
        return Err(SdkError::ResponseTooLarge {
            limit: MAX_RESPONSE_BYTES,
        });
    }
    Ok(())
}

async fn collect_bounded(mut body: Incoming) -> Result<Bytes, SdkError> {
    let mut collected = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| SdkError::Transport)?;
        if let Ok(data) = frame.into_data() {
            let new_len =
                collected
                    .len()
                    .checked_add(data.len())
                    .ok_or(SdkError::ResponseTooLarge {
                        limit: MAX_RESPONSE_BYTES,
                    })?;
            if new_len > MAX_RESPONSE_BYTES {
                return Err(SdkError::ResponseTooLarge {
                    limit: MAX_RESPONSE_BYTES,
                });
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
        Command, DesktopProbeCommand, EffectStage, ErrorCode, ProtocolVersion, RequestId,
        RetryAdvice, Timestamp,
    };

    use super::*;

    type TestError = Box<dyn Error + Send + Sync>;

    #[test]
    fn credentials_are_redacted_and_base_uri_is_strict() -> Result<(), TestError> {
        let secret = "sdk-secret-canary-0123456789abcdef";
        let token = BearerToken::new(secret)?;
        let client = Client::new("http://127.0.0.1:8080", secret)?;
        assert!(BaseUri::parse("http://[::1]:8080").is_ok());

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
            "https://127.0.0.1",
            "http://127.0.0.1/api",
            "http://127.0.0.1/?tenant=one",
            "http://user@127.0.0.1",
            "http://localhost:8080",
            "http://192.0.2.1:8080",
            "127.0.0.1:8080",
        ] {
            assert!(matches!(
                BaseUri::parse(invalid),
                Err(SdkError::InvalidBaseUri)
            ));
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
