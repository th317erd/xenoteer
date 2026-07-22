//! Bearer-token authentication boundary with digest-only runtime storage.

use std::{
    collections::BTreeSet,
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Take},
    net::SocketAddr,
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        io::RawFd,
    },
    path::Path,
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use xenoteer_protocol::RequestId;

use crate::{
    abuse::{AbuseControls, AuthenticationSource},
    problem::ApiProblem,
};

/// Minimum token material accepted by the built-in provider.
pub const MIN_TOKEN_BYTES: usize = 32;
/// Maximum token material accepted from a header or token file.
pub const MAX_TOKEN_BYTES: usize = 1_024;
// Read one byte beyond the largest accepted token plus CRLF so a valid-looking
// prefix can never hide trailing file or FIFO bytes.
const TOKEN_FILE_READ_LIMIT: u64 = (MAX_TOKEN_BYTES as u64) + 3;
const INHERITED_TOKEN_FD: RawFd = 9;
const INHERITED_TOKEN_PATH: &str = "/proc/self/fd/9";

/// A stable authorization grant attached to an authenticated principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Grant {
    /// Read detailed desktop status and capability reports.
    #[serde(rename = "desktop:status")]
    DesktopStatus,
    /// Read desktop/process metadata and harmless events.
    #[serde(rename = "desktop:observe")]
    DesktopObserve,
    /// Acquire a lease and submit physical input.
    #[serde(rename = "input:control")]
    InputControl,
    /// Launch a configured application profile.
    #[serde(rename = "application:launch")]
    ApplicationLaunch,
    /// Terminate a managed application process group.
    #[serde(rename = "application:terminate")]
    ApplicationTerminate,
}

impl Grant {
    /// Returns the stable capability string used by policy and the wire protocol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DesktopStatus => "desktop:status",
            Self::DesktopObserve => "desktop:observe",
            Self::InputControl => "input:control",
            Self::ApplicationLaunch => "application:launch",
            Self::ApplicationTerminate => "application:terminate",
        }
    }

    /// Parses one member of the closed release-three grant vocabulary.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::operator_grants()
            .into_iter()
            .find(|grant| grant.as_str() == name)
    }

    /// Returns the complete release-three operator grant set.
    #[must_use]
    pub const fn operator_grants() -> [Self; 5] {
        [
            Self::DesktopStatus,
            Self::DesktopObserve,
            Self::InputControl,
            Self::ApplicationLaunch,
            Self::ApplicationTerminate,
        ]
    }
}

/// Authenticated identity and its prevalidated grants.
#[derive(Clone, PartialEq, Eq)]
pub struct Principal {
    id: Arc<str>,
    grants: Arc<BTreeSet<Grant>>,
}

impl Principal {
    /// Builds a bounded public identity.
    pub fn new(
        id: impl Into<String>,
        grants: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, PrincipalError> {
        let id = id.into();
        if id.is_empty()
            || id.len() > 128
            || !id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
            })
        {
            return Err(PrincipalError::InvalidId);
        }
        Ok(Self {
            id: Arc::from(id),
            grants: Arc::new(grants.into_iter().collect()),
        })
    }

    /// Builds the local release-three operator used by a single-token deployment.
    pub fn local_operator() -> Result<Self, PrincipalError> {
        Self::new("local-operator", Grant::operator_grants())
    }

    /// Returns the non-secret public principal identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns whether this principal carries an exact grant.
    #[must_use]
    pub fn has_grant(&self, grant: Grant) -> bool {
        self.grants.contains(&grant)
    }

    /// Returns grants in deterministic wire order.
    pub(crate) fn grant_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.grants.iter().map(|grant| grant.as_str())
    }
}

impl fmt::Debug for Principal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Principal")
            .field("id", &self.id)
            .field("grants", &self.grants)
            .finish()
    }
}

/// A SHA-256 token fingerprint that cannot reveal its bytes through formatting.
#[derive(Clone)]
pub struct SecretFingerprint([u8; 32]);

impl SecretFingerprint {
    fn from_token(pepper: &SecretPepper, token: &[u8]) -> Result<Self, TokenMaterialError> {
        if !(MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&token.len()) {
            return Err(TokenMaterialError::Length);
        }
        if !is_bearer_token(token) {
            return Err(TokenMaterialError::Syntax);
        }
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&pepper.0)
            .map_err(|_| TokenMaterialError::KeyedDigest)?;
        mac.update(b"xenoteer.static-bearer-token.v1\0");
        mac.update(token);
        let digest = mac.finalize().into_bytes();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        Ok(Self(bytes))
    }

    fn matches(&self, candidate: &Self) -> bool {
        bool::from(self.0.ct_eq(&candidate.0))
    }
}

#[derive(Clone)]
struct SecretPepper([u8; 32]);

impl SecretPepper {
    fn generate() -> Result<Self, TokenProviderError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| TokenProviderError::Entropy)?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for SecretPepper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretPepper(<redacted>)")
    }
}

impl fmt::Debug for SecretFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretFingerprint(<redacted>)")
    }
}

/// Pluggable digest-to-principal lookup used by HTTP and WebSocket middleware.
pub trait TokenProvider: Send + Sync + 'static {
    /// Fingerprints and resolves one ephemeral presented token.
    ///
    /// Implementations must not retain or log the plaintext slice.
    fn authenticate(&self, token: &[u8]) -> Option<Principal>;
}

#[derive(Clone)]
struct TokenRecord {
    fingerprint: SecretFingerprint,
    principal: Principal,
}

/// Immutable in-memory provider supporting overlapping rotation records.
#[derive(Clone)]
pub struct StaticTokenProvider {
    pepper: SecretPepper,
    records: Arc<[TokenRecord]>,
}

impl StaticTokenProvider {
    /// Builds one digest-only token record.
    pub fn single(token: &[u8], principal: Principal) -> Result<Self, TokenProviderError> {
        Self::from_records([(token, principal)])
    }

    /// Builds multiple records and rejects duplicate fingerprints.
    pub fn from_records<'a>(
        records: impl IntoIterator<Item = (&'a [u8], Principal)>,
    ) -> Result<Self, TokenProviderError> {
        let pepper = SecretPepper::generate()?;
        let mut checked: Vec<TokenRecord> = Vec::new();
        for (token, principal) in records {
            let fingerprint = SecretFingerprint::from_token(&pepper, token)?;
            if checked
                .iter()
                .any(|existing| existing.fingerprint.matches(&fingerprint))
            {
                return Err(TokenProviderError::DuplicateFingerprint);
            }
            checked.push(TokenRecord {
                fingerprint,
                principal,
            });
        }
        if checked.is_empty() {
            return Err(TokenProviderError::Empty);
        }
        Ok(Self {
            pepper,
            records: Arc::from(checked),
        })
    }

    /// Loads one bounded regular file or the fixed inherited FIFO descriptor.
    ///
    /// Owner-readable files with no group/other permissions are accepted. This
    /// supports both runtime-created `0600` files and read-only `0400` secret
    /// mounts. The path and token bytes are never retained in the provider.
    pub fn from_file(path: &Path, principal: Principal) -> Result<Self, TokenLoadError> {
        let inherited = path == Path::new(INHERITED_TOKEN_PATH);
        let _inherited_guard = inherited.then(InheritedTokenFdGuard::new);
        let mut file = if inherited {
            duplicate_inherited_token_fd()?
        } else {
            File::open(path).map_err(|_| TokenLoadError::Open)?
        };
        let metadata = file.metadata().map_err(|_| TokenLoadError::Metadata)?;
        if !(metadata.is_file() || (inherited && metadata.file_type().is_fifo())) {
            return Err(TokenLoadError::NotRegularFile);
        }
        validate_secret_file_mode(&metadata)?;

        let mut token = Vec::new();
        let mut bounded: Take<&mut File> = file.by_ref().take(TOKEN_FILE_READ_LIMIT);
        if bounded.read_to_end(&mut token).is_err() {
            token.fill(0);
            return Err(TokenLoadError::Read);
        }
        normalize_text_file_ending(&mut token);
        let provider = Self::single(&token, principal).map_err(TokenLoadError::Provider);
        token.fill(0);
        provider
    }
}

fn duplicate_inherited_token_fd() -> Result<File, TokenLoadError> {
    open_inherited_token_path(Path::new(INHERITED_TOKEN_PATH))
}

fn open_inherited_token_path(path: &Path) -> Result<File, TokenLoadError> {
    // The launcher waits for the pipe writer to close before exec. A normal
    // FIFO open would consequently wait forever for a new writer; O_NONBLOCK
    // duplicates the already-open procfs descriptor immediately. Reads still
    // consume every buffered byte and then observe EOF.
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| TokenLoadError::Open)
}

/// Cloneable authentication middleware state.
#[derive(Clone)]
pub(crate) struct AuthenticationState {
    authentication: Authentication,
    abuse: AbuseControls,
}

impl AuthenticationState {
    pub(crate) fn new(authentication: Authentication, abuse: AbuseControls) -> Self {
        Self {
            authentication,
            abuse,
        }
    }
}

/// Authenticates a versioned request and installs only non-secret extensions.
pub(crate) async fn require_authentication(
    State(authentication): State<AuthenticationState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or_default();
    request.extensions_mut().insert(request_id);
    let source = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(AuthenticationSource::Fallback, |ConnectInfo(address)| {
            AuthenticationSource::Ip(address.ip())
        });
    if !authentication.abuse.authentication_preflight(source) {
        return ApiProblem::resource_exhausted(request_id).into_response();
    }
    let mut authorization = request.headers().get_all(header::AUTHORIZATION).iter();
    let header = authorization.next().map(axum::http::HeaderValue::as_bytes);
    if authorization.next().is_some() {
        return failed_authentication(&authentication.abuse, source, request_id);
    }
    let Some(principal) = authentication.authentication.authenticate_header(header) else {
        return failed_authentication(&authentication.abuse, source, request_id);
    };
    // Authentication is the final component allowed to observe the plaintext
    // credential. Downstream handlers, extensions, and middleware receive only
    // the resolved non-secret principal.
    request.headers_mut().remove(header::AUTHORIZATION);
    request.extensions_mut().insert(principal);
    next.run(request).await
}

fn failed_authentication(
    abuse: &AbuseControls,
    source: AuthenticationSource,
    request_id: RequestId,
) -> Response {
    if abuse.record_authentication_failure(source) {
        ApiProblem::authentication_required(request_id).into_response()
    } else {
        ApiProblem::resource_exhausted(request_id).into_response()
    }
}

impl TokenProvider for StaticTokenProvider {
    fn authenticate(&self, token: &[u8]) -> Option<Principal> {
        let fingerprint = SecretFingerprint::from_token(&self.pepper, token).ok()?;
        let mut matched = None;
        // Always compare every record so a valid record's position does not
        // determine the number of digest comparisons.
        for record in self.records.iter() {
            if record.fingerprint.matches(&fingerprint) {
                matched = Some(record.principal.clone());
            }
        }
        matched
    }
}

/// Closes the fixed inherited token descriptor on every startup outcome.
///
/// The daemon duplicates inherited descriptor 9 without reopening its FIFO.
/// Closing the original here removes the last descriptor that a compromised
/// desktop process could ever try to reach through the daemon's procfs tree.
struct InheritedTokenFdGuard(Option<RawFd>);

impl InheritedTokenFdGuard {
    const fn new() -> Self {
        Self(Some(INHERITED_TOKEN_FD))
    }
}

impl Drop for InheritedTokenFdGuard {
    fn drop(&mut self) {
        if let Some(fd) = self.0.take() {
            let _ignored = nix::unistd::close(fd);
        }
    }
}

impl fmt::Debug for StaticTokenProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticTokenProvider")
            .field("record_count", &self.records.len())
            .finish()
    }
}

/// Authentication policy for protected versioned routes.
#[derive(Clone)]
pub enum Authentication {
    /// Require a Bearer credential and resolve it through a token provider.
    Bearer(Arc<dyn TokenProvider>),
    /// Explicit loopback-only development bypass validated by daemon config.
    InsecureDevelopment(Principal),
}

impl Authentication {
    /// Creates a protected Bearer policy.
    #[must_use]
    pub fn bearer(provider: impl TokenProvider) -> Self {
        Self::Bearer(Arc::new(provider))
    }

    /// Creates the explicit development bypass policy.
    #[must_use]
    pub fn insecure_development(principal: Principal) -> Self {
        Self::InsecureDevelopment(principal)
    }

    pub(crate) fn authenticate_header(&self, header: Option<&[u8]>) -> Option<Principal> {
        match self {
            Self::InsecureDevelopment(principal) => Some(principal.clone()),
            Self::Bearer(provider) => {
                let token = parse_bearer(header?)?;
                provider.authenticate(token)
            }
        }
    }
}

impl fmt::Debug for Authentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer(_) => formatter.write_str("Authentication::Bearer(<provider>)"),
            Self::InsecureDevelopment(principal) => formatter
                .debug_tuple("Authentication::InsecureDevelopment")
                .field(principal)
                .finish(),
        }
    }
}

fn parse_bearer(header: &[u8]) -> Option<&[u8]> {
    const PREFIX_LEN: usize = 7;
    if header.len() <= PREFIX_LEN || !header[..PREFIX_LEN].eq_ignore_ascii_case(b"Bearer ") {
        return None;
    }
    let token = &header[PREFIX_LEN..];
    if token.len() > MAX_TOKEN_BYTES || !is_bearer_token(token) {
        return None;
    }
    Some(token)
}

/// RFC 6750 Bearer credentials use the RFC 7235 `token68` alphabet. Padding is
/// allowed only as a suffix so every accepted token file can be represented in
/// an Authorization header without an encoding-dependent surprise at startup.
fn is_bearer_token(token: &[u8]) -> bool {
    let unpadded_len = token
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(token.len());
    unpadded_len != 0
        && token[..unpadded_len].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
        && token[unpadded_len..].iter().all(|byte| *byte == b'=')
}

fn normalize_text_file_ending(token: &mut Vec<u8>) {
    let newline_bytes = if token.ends_with(b"\r\n") {
        2
    } else if token.ends_with(b"\n") {
        1
    } else {
        0
    };
    if newline_bytes != 0
        && token[..token.len() - newline_bytes]
            .iter()
            .all(u8::is_ascii_graphic)
    {
        token.truncate(token.len() - newline_bytes);
    }
}

#[cfg(unix)]
fn validate_secret_file_mode(metadata: &std::fs::Metadata) -> Result<(), TokenLoadError> {
    use std::os::unix::fs::MetadataExt;

    let mode = metadata.mode() & 0o777;
    if mode & 0o400 == 0 || mode & 0o077 != 0 {
        return Err(TokenLoadError::Permissions);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secret_file_mode(_: &std::fs::Metadata) -> Result<(), TokenLoadError> {
    Err(TokenLoadError::UnsupportedPlatform)
}

/// Principal construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrincipalError {
    /// Public principal IDs are bounded visible ASCII identifiers.
    #[error("principal identifier is invalid")]
    InvalidId,
}

/// Token material validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenMaterialError {
    /// Token material must contain between 32 and 1024 bytes.
    #[error("token material has invalid length")]
    Length,
    /// Token bytes must be directly usable as an RFC `token68` credential.
    #[error("token material is not valid Bearer token68 text")]
    Syntax,
    /// The fixed-size process pepper could not initialize the keyed digest.
    #[error("could not initialize keyed token digest")]
    KeyedDigest,
}

/// Static provider construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenProviderError {
    /// No token records were supplied.
    #[error("token provider requires at least one record")]
    Empty,
    /// More than one record contained the same token fingerprint.
    #[error("token provider contains a duplicate fingerprint")]
    DuplicateFingerprint,
    /// The operating system could not provide a process-local pepper.
    #[error("could not initialize authentication entropy")]
    Entropy,
    /// One record contained invalid token material.
    #[error(transparent)]
    Material(#[from] TokenMaterialError),
}

/// Secret-safe token-file loading failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenLoadError {
    /// The configured file could not be opened.
    #[error("could not open authentication token file")]
    Open,
    /// File metadata could not be read.
    #[error("could not inspect authentication token file")]
    Metadata,
    /// The opened object is neither a regular file nor the fixed inherited FIFO.
    #[error("authentication token source has an unsupported file type")]
    NotRegularFile,
    /// Owner read is absent or group/other permissions are present.
    #[error("authentication token file permissions must be 0400 or 0600")]
    Permissions,
    /// Secure mode checks are unavailable on this target.
    #[cfg(not(unix))]
    #[error("authentication token file mode checks require Unix")]
    UnsupportedPlatform,
    /// The bounded token read failed.
    #[error("could not read authentication token file")]
    Read,
    /// Token provider validation failed.
    #[error(transparent)]
    Provider(#[from] TokenProviderError),
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{io::Write, os::fd::AsRawFd};

    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, Request, StatusCode},
        middleware,
        routing::get,
    };
    use tower::ServiceExt;

    use super::*;

    fn token(byte: u8) -> [u8; MIN_TOKEN_BYTES] {
        [byte; MIN_TOKEN_BYTES]
    }

    #[test]
    fn fingerprints_and_provider_debug_are_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let canary = b"AUTH_TOKEN_SECRET_CANARY_00000000";
        let pepper = SecretPepper([1; 32]);
        let fingerprint = SecretFingerprint::from_token(&pepper, canary)?;
        let provider = StaticTokenProvider::single(canary, Principal::local_operator()?)?;
        assert!(!format!("{fingerprint:?}").contains("AUTH_TOKEN_SECRET_CANARY"));
        assert!(!format!("{provider:?}").contains("AUTH_TOKEN_SECRET_CANARY"));
        Ok(())
    }

    #[test]
    fn provider_accepts_only_the_matching_digest() -> Result<(), Box<dyn std::error::Error>> {
        let valid = token(b'v');
        let invalid = token(b'x');
        let provider = StaticTokenProvider::single(&valid, Principal::local_operator()?)?;
        assert!(provider.authenticate(&valid).is_some());
        assert!(provider.authenticate(&invalid).is_none());
        Ok(())
    }

    #[test]
    fn grant_names_round_trip_through_closed_vocabulary() {
        for grant in Grant::operator_grants() {
            assert_eq!(Grant::from_name(grant.as_str()), Some(grant));
        }
        assert_eq!(Grant::from_name("desktop:administrator"), None);
    }

    #[test]
    fn bearer_parser_is_strict_and_scheme_is_case_insensitive() {
        let valid = b"a".repeat(MIN_TOKEN_BYTES);
        let mut header = b"bEaReR ".to_vec();
        header.extend_from_slice(&valid);
        assert_eq!(parse_bearer(&header), Some(valid.as_slice()));
        assert_eq!(parse_bearer(b"Bearer token with whitespace"), None);
        assert_eq!(parse_bearer(b"Bearer token=padding=inside"), None);
        assert_eq!(parse_bearer(b"Bearer token=="), Some(&b"token=="[..]));
        assert_eq!(parse_bearer(b"Basic abc"), None);
    }

    #[test]
    fn provider_rejects_token_material_that_cannot_cross_http()
    -> Result<(), Box<dyn std::error::Error>> {
        let binary = [0_u8; MIN_TOKEN_BYTES];
        assert!(matches!(
            StaticTokenProvider::single(&binary, Principal::local_operator()?),
            Err(TokenProviderError::Material(TokenMaterialError::Syntax))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_request_strips_plaintext_credential_before_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn downstream(headers: HeaderMap) -> StatusCode {
            if headers.contains_key(header::AUTHORIZATION) {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::NO_CONTENT
            }
        }

        let valid = token(b'v');
        let provider = StaticTokenProvider::single(&valid, Principal::local_operator()?)?;
        let abuse = AbuseControls::new();
        let application =
            Router::new()
                .route("/", get(downstream))
                .layer(middleware::from_fn_with_state(
                    AuthenticationState::new(Authentication::bearer(provider), abuse),
                    require_authentication,
                ));
        let response = application
            .oneshot(
                Request::get("/")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", std::str::from_utf8(&valid)?),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn token_loader_rejects_valid_maximum_prefix_with_trailing_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("xenoteer-token-loader-{}", uuid::Uuid::new_v4()));
        let mut contents = vec![b'a'; MAX_TOKEN_BYTES];
        contents.extend_from_slice(b"\r\ntrailing");
        std::fs::write(&path, contents)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let result = StaticTokenProvider::from_file(&path, Principal::local_operator()?);
        std::fs::remove_file(&path)?;

        assert!(matches!(
            result,
            Err(TokenLoadError::Provider(TokenProviderError::Material(
                TokenMaterialError::Length
            )))
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn inherited_pipe_is_duplicated_after_its_writer_closes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (reader, writer) = nix::unistd::pipe()?;
        let expected = token(b'p');
        let mut writer = File::from(writer);
        writer.write_all(&expected)?;
        drop(writer);

        let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", reader.as_raw_fd()));
        let mut duplicate = open_inherited_token_path(&path)?;
        let mut observed = Vec::new();
        duplicate.read_to_end(&mut observed)?;

        assert_eq!(observed, expected);
        Ok(())
    }
}
