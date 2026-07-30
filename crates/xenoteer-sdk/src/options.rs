//! Validated public client configuration and redacted diagnostics.

use std::{
    fmt,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_util::FutureExt;
use rustls::ClientConfig;
use xenoteer_protocol::VersionRange;

use crate::{BaseUri, BearerToken, SdkError};

/// Default deadline for opening one HTTP or WebSocket connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum caller-configurable connection deadline.
pub const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum caller-configurable request deadline.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Maximum caller-configurable reconnect attempts after an established socket fails.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 100;
/// Maximum caller-configurable reconnect delay.
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

const DEFAULT_RECONNECT_ATTEMPTS: u32 = 5;
const DEFAULT_RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(200);
const DEFAULT_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(10);
const MAX_TLS_ROOTS: usize = 32;
const MAX_CERTIFICATE_DER_BYTES: usize = 64 * 1024;
const MAX_CLIENT_CERTIFICATES: usize = 8;
const MAX_PRIVATE_KEY_DER_BYTES: usize = 64 * 1024;

type TokenFuture = Pin<Box<dyn Future<Output = Result<BearerToken, SdkError>> + Send + 'static>>;

pub(crate) trait ResolveToken: Send + Sync {
    fn validate(&self) -> Result<(), SdkError>;
    fn resolve(&self) -> TokenFuture;
}

struct StaticToken {
    token: Option<BearerToken>,
}

impl ResolveToken for StaticToken {
    fn validate(&self) -> Result<(), SdkError> {
        self.token
            .as_ref()
            .map(|_| ())
            .ok_or(SdkError::InvalidBearerToken)
    }

    fn resolve(&self) -> TokenFuture {
        let token = self.token.clone();
        Box::pin(async move { token.ok_or(SdkError::InvalidBearerToken) })
    }
}

struct CallbackToken<F> {
    callback: F,
}

impl<F, Fut, T, E> ResolveToken for CallbackToken<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: AsRef<[u8]> + Send + 'static,
    E: Send + 'static,
{
    fn validate(&self) -> Result<(), SdkError> {
        Ok(())
    }

    fn resolve(&self) -> TokenFuture {
        let future = catch_unwind(AssertUnwindSafe(|| (self.callback)()));
        Box::pin(async move {
            let future = future.map_err(|_| SdkError::TokenProvider)?;
            let value = AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .map_err(|_| SdkError::TokenProvider)?
                .map_err(|_| SdkError::TokenProvider)?;
            BearerToken::new(value)
        })
    }
}

/// Bounded event reconnection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub(crate) max_attempts: u32,
    pub(crate) initial_delay: Duration,
    pub(crate) max_delay: Duration,
}

impl ReconnectPolicy {
    /// Creates a non-zero bounded exponential-backoff policy.
    pub fn new(
        max_attempts: u32,
        initial_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, SdkError> {
        if !(1..=MAX_RECONNECT_ATTEMPTS).contains(&max_attempts)
            || initial_delay.is_zero()
            || initial_delay > max_delay
            || max_delay > MAX_RECONNECT_DELAY
        {
            return Err(SdkError::InvalidRequest);
        }
        Ok(Self {
            max_attempts,
            initial_delay,
            max_delay,
        })
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RECONNECT_ATTEMPTS,
            initial_delay: DEFAULT_RECONNECT_INITIAL_DELAY,
            max_delay: DEFAULT_RECONNECT_MAX_DELAY,
        }
    }
}

#[derive(Clone)]
struct ClientIdentity {
    certificate_chain_der: Vec<Vec<u8>>,
    private_key_der: Vec<u8>,
}

/// One Rustls policy shared by the HTTP and WebSocket connectors.
#[derive(Clone)]
pub struct TlsPolicy {
    pub(crate) use_native_roots: bool,
    pub(crate) root_certificates_der: Vec<Vec<u8>>,
    client_identity: Option<ClientIdentity>,
}

impl TlsPolicy {
    /// Uses platform-native roots. This requires the `native-roots` feature.
    #[must_use]
    pub const fn native_roots() -> Self {
        Self {
            use_native_roots: true,
            root_certificates_der: Vec::new(),
            client_identity: None,
        }
    }

    /// Uses only caller-supplied trust anchors.
    #[must_use]
    pub const fn custom_roots() -> Self {
        Self {
            use_native_roots: false,
            root_certificates_der: Vec::new(),
            client_identity: None,
        }
    }

    /// Adds one DER-encoded trust anchor.
    #[must_use]
    pub fn with_root_certificate_der(mut self, certificate_der: impl Into<Vec<u8>>) -> Self {
        self.root_certificates_der.push(certificate_der.into());
        self
    }

    /// Configures one DER certificate chain and PKCS#1, PKCS#8, or SEC1 private key.
    #[must_use]
    pub fn with_client_identity_der(
        mut self,
        certificate_chain_der: Vec<Vec<u8>>,
        private_key_der: impl Into<Vec<u8>>,
    ) -> Self {
        self.client_identity = Some(ClientIdentity {
            certificate_chain_der,
            private_key_der: private_key_der.into(),
        });
        self
    }

    pub(crate) fn validate(&self) -> Result<(), SdkError> {
        if self.root_certificates_der.len() > MAX_TLS_ROOTS
            || self.root_certificates_der.iter().any(|certificate| {
                certificate.is_empty() || certificate.len() > MAX_CERTIFICATE_DER_BYTES
            })
            || (!self.use_native_roots && self.root_certificates_der.is_empty())
        {
            return Err(SdkError::TlsConfiguration);
        }
        if let Some(identity) = &self.client_identity
            && (identity.certificate_chain_der.is_empty()
                || identity.certificate_chain_der.len() > MAX_CLIENT_CERTIFICATES
                || identity.certificate_chain_der.iter().any(|certificate| {
                    certificate.is_empty() || certificate.len() > MAX_CERTIFICATE_DER_BYTES
                })
                || identity.private_key_der.is_empty()
                || identity.private_key_der.len() > MAX_PRIVATE_KEY_DER_BYTES)
        {
            return Err(SdkError::TlsConfiguration);
        }
        Ok(())
    }

    pub(crate) fn client_identity(&self) -> Option<(&[Vec<u8>], &[u8])> {
        self.client_identity.as_ref().map(|identity| {
            (
                identity.certificate_chain_der.as_slice(),
                identity.private_key_der.as_slice(),
            )
        })
    }
}

impl fmt::Debug for TlsPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsPolicy")
            .field("use_native_roots", &self.use_native_roots)
            .field("custom_root_count", &self.root_certificates_der.len())
            .field("client_identity", &self.client_identity.is_some())
            .finish()
    }
}

/// Transport class for one safe diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeLogTransport {
    /// Retry-neutral HTTP.
    Http,
    /// Initial or reconnecting WebSocket.
    WebSocket,
}

/// Closed operation category for one safe diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeLogOperation {
    /// Resolve a static or callback bearer token.
    TokenResolution,
    /// Perform exactly one HTTP exchange.
    HttpExchange,
    /// Open the initial event WebSocket.
    WebSocketConnect,
    /// Reopen an interrupted event WebSocket.
    WebSocketReconnect,
}

/// Closed outcome category for one safe diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeLogOutcome {
    /// The bounded operation began.
    Started,
    /// The bounded operation succeeded.
    Succeeded,
    /// The bounded operation failed.
    Failed,
}

/// Metadata-only diagnostic event. It deliberately has no free-form text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeLogEvent {
    /// Transport being observed.
    pub transport: SafeLogTransport,
    /// Bounded operation being observed.
    pub operation: SafeLogOperation,
    /// Stable outcome.
    pub outcome: SafeLogOutcome,
}

/// A caller log hook declined a diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeLogHookError;

type SafeLogHook = dyn Fn(SafeLogEvent) -> Result<(), SafeLogHookError> + Send + Sync + 'static;

/// Validated immutable connection options.
#[derive(Clone)]
pub struct ClientOptions {
    pub(crate) base: BaseUri,
    pub(crate) token: Arc<dyn ResolveToken>,
    pub(crate) tls_policy: TlsPolicy,
    pub(crate) tls_config: Arc<ClientConfig>,
    pub(crate) connect_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) reconnect_policy: ReconnectPolicy,
    pub(crate) client_name: Arc<str>,
    pub(crate) client_version: Arc<str>,
    pub(crate) protocol_range: VersionRange,
    safe_log: Option<Arc<SafeLogHook>>,
}

impl ClientOptions {
    /// Begins a validated configuration using one static bearer token.
    #[must_use]
    pub fn builder(
        base_uri: impl AsRef<str>,
        bearer_token: impl AsRef<[u8]>,
    ) -> ClientOptionsBuilder {
        ClientOptionsBuilder::with_token(
            base_uri.as_ref().to_owned(),
            Arc::new(StaticToken {
                token: BearerToken::new(bearer_token).ok(),
            }),
        )
    }

    /// Begins a validated configuration using an asynchronous rotating token provider.
    #[must_use]
    pub fn builder_with_token_provider<F, Fut, T, E>(
        base_uri: impl AsRef<str>,
        provider: F,
    ) -> ClientOptionsBuilder
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        T: AsRef<[u8]> + Send + 'static,
        E: Send + 'static,
    {
        ClientOptionsBuilder::with_token(
            base_uri.as_ref().to_owned(),
            Arc::new(CallbackToken { callback: provider }),
        )
    }

    pub(crate) fn safe_log(&self, event: SafeLogEvent) {
        if let Some(hook) = &self.safe_log {
            let _ignored = catch_unwind(AssertUnwindSafe(|| hook(event)));
        }
    }
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientOptions")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("tls_policy", &self.tls_policy)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("reconnect_policy", &self.reconnect_policy)
            .field("client_name", &self.client_name)
            .field("client_version", &self.client_version)
            .field("protocol_range", &self.protocol_range)
            .field("safe_log", &self.safe_log.is_some())
            .finish()
    }
}

/// Builder for one validated immutable [`ClientOptions`] value.
pub struct ClientOptionsBuilder {
    base_uri: String,
    token: Arc<dyn ResolveToken>,
    tls_policy: TlsPolicy,
    connect_timeout: Duration,
    request_timeout: Duration,
    reconnect_policy: ReconnectPolicy,
    client_name: String,
    client_version: String,
    protocol_range: VersionRange,
    safe_log: Option<Arc<SafeLogHook>>,
}

impl ClientOptionsBuilder {
    fn with_token(base_uri: String, token: Arc<dyn ResolveToken>) -> Self {
        Self {
            base_uri,
            token,
            tls_policy: TlsPolicy::native_roots(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: crate::DEFAULT_REQUEST_TIMEOUT,
            reconnect_policy: ReconnectPolicy::default(),
            client_name: "xenoteer-sdk-rust".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_range: VersionRange::V1,
            safe_log: None,
        }
    }

    /// Sets the deadline for opening one HTTP or WebSocket connection.
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets one end-to-end deadline for credential resolution plus the HTTP
    /// request and complete response body.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sets the bounded WebSocket reconnection policy.
    #[must_use]
    pub fn reconnect_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.reconnect_policy = policy;
        self
    }

    /// Sets safe client metadata reported in the WebSocket hello.
    #[must_use]
    pub fn client_metadata(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_name = name.into();
        self.client_version = version.into();
        self
    }

    /// Sets the inclusive supported protocol range used for negotiation.
    #[must_use]
    pub fn protocol_range(mut self, range: VersionRange) -> Self {
        self.protocol_range = range;
        self
    }

    /// Sets one trust/client-identity policy shared by HTTP and WebSocket.
    #[must_use]
    pub fn tls_policy(mut self, policy: TlsPolicy) -> Self {
        self.tls_policy = policy;
        self
    }

    /// Installs a metadata-only diagnostic hook.
    #[must_use]
    pub fn safe_log<F>(mut self, hook: F) -> Self
    where
        F: Fn(SafeLogEvent) -> Result<(), SafeLogHookError> + Send + Sync + 'static,
    {
        self.safe_log = Some(Arc::new(hook));
        self
    }

    /// Validates and freezes the complete public configuration.
    pub fn build(self) -> Result<ClientOptions, SdkError> {
        self.token.validate()?;
        if self.connect_timeout.is_zero()
            || self.connect_timeout > MAX_CONNECT_TIMEOUT
            || self.request_timeout.is_zero()
            || self.request_timeout > MAX_REQUEST_TIMEOUT
            || !valid_metadata(&self.client_name)
            || !valid_metadata(&self.client_version)
            || self.protocol_range.validate().is_err()
            || self.protocol_range.major() != VersionRange::V1.major()
        {
            return Err(SdkError::InvalidRequest);
        }
        self.tls_policy.validate()?;
        let tls_config = Arc::new(crate::transport::build_tls_config(&self.tls_policy)?);
        Ok(ClientOptions {
            base: BaseUri::parse(&self.base_uri)?,
            token: self.token,
            tls_policy: self.tls_policy,
            tls_config,
            connect_timeout: self.connect_timeout,
            request_timeout: self.request_timeout,
            reconnect_policy: self.reconnect_policy,
            client_name: Arc::from(self.client_name),
            client_version: Arc::from(self.client_version),
            protocol_range: self.protocol_range,
            safe_log: self.safe_log,
        })
    }
}

fn valid_metadata(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
