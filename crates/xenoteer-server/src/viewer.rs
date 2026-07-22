//! Origin-bound, single-use viewer-ticket issuance and consumption.

use core::fmt;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, MAX_VIEWER_TICKET_TTL_SECONDS, OneTimeViewerTicket, RequestId,
    Timestamp, ViewerMode, ViewerOrigin, ViewerPrincipalId, ViewerTicketAudience,
    ViewerTicketRequest, ViewerTicketSecret, ViewerTicketUsePolicy,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{ControlPlaneError, control_problem},
    problem::ApiProblem,
};

/// Default maximum number of live one-time tickets retained in memory.
pub const DEFAULT_VIEWER_TICKET_CAPACITY: usize = 1_024;
/// Defensive upper bound for configured live ticket capacity.
pub const MAX_VIEWER_TICKET_CAPACITY: usize = 16_384;
/// Stable audience text used at the future viewer WebSocket gateway boundary.
pub const VIEWER_WEBSOCKET_AUDIENCE: &str = "viewer_websocket";

const TICKET_BYTES: usize = 32;
const DIGEST_DOMAIN: &[u8] = b"xenoteer.viewer-ticket.v1\0";
const CACHE_CONTROL_PRIVATE_NO_STORE: &str = "private, no-store";

/// Boxed future used by the object-safe viewer-ticket service boundary.
pub type ViewerTicketFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Authenticated, route-fenced ticket issuance context.
#[derive(Debug, Clone)]
pub struct ViewerTicketIssueContext {
    principal: Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    origin: ViewerOrigin,
    audience: ViewerTicketAudience,
}

impl ViewerTicketIssueContext {
    fn new(
        principal: Principal,
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        origin: ViewerOrigin,
    ) -> Self {
        Self {
            principal,
            request_id,
            desktop_id,
            desktop_generation,
            origin,
            audience: ViewerTicketAudience::ViewerWebsocket,
        }
    }

    /// Returns the authenticated API principal.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the HTTP request correlation identifier.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Returns the exact desktop resource from the route.
    #[must_use]
    pub const fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    /// Returns the current generation proven by admission.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns the canonical allowlisted browser origin.
    #[must_use]
    pub const fn origin(&self) -> &ViewerOrigin {
        &self.origin
    }

    /// Returns the sole release-one ticket audience.
    #[must_use]
    pub const fn audience(&self) -> ViewerTicketAudience {
        self.audience
    }
}

/// Object-safe seam between HTTP admission and viewer ticket issuance.
pub trait ViewerTicketService: Send + Sync + 'static {
    /// Issues a ticket only after independently revalidating principal, scope,
    /// generation, origin, audience, capacity, and lifetime policy.
    fn issue<'a>(
        &'a self,
        context: ViewerTicketIssueContext,
        request: ViewerTicketRequest,
    ) -> ViewerTicketFuture<'a, Result<OneTimeViewerTicket, ControlPlaneError>>;

    /// Atomically authenticates and consumes one browser-presented ticket.
    ///
    /// The one-time secret authenticates lookup and returns the principal claim
    /// retained at issuance. Browser callers never need a second long-lived
    /// bearer credential at the WebSocket boundary.
    fn consume_for_gateway<'a>(
        &'a self,
        _request: ViewerTicketConsumeRequest,
    ) -> ViewerTicketFuture<'a, Result<ViewerTicketClaims, ControlPlaneError>> {
        Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
    }
}

pub(crate) type SharedViewerTicketService = Arc<dyn ViewerTicketService>;

#[derive(Debug)]
pub(crate) struct UnavailableViewerTicketService;

impl ViewerTicketService for UnavailableViewerTicketService {
    fn issue<'a>(
        &'a self,
        _: ViewerTicketIssueContext,
        _: ViewerTicketRequest,
    ) -> ViewerTicketFuture<'a, Result<OneTimeViewerTicket, ControlPlaneError>> {
        Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
    }
}

/// Bounded in-memory registry settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerTicketRegistryConfig {
    capacity: usize,
    ttl: Duration,
}

impl ViewerTicketRegistryConfig {
    /// Creates a non-empty bounded registry with a TTL no greater than 60s.
    pub fn new(capacity: usize, ttl: Duration) -> Result<Self, ViewerTicketRegistryError> {
        if capacity == 0
            || capacity > MAX_VIEWER_TICKET_CAPACITY
            || ttl.is_zero()
            || ttl > Duration::from_secs(MAX_VIEWER_TICKET_TTL_SECONDS as u64)
        {
            return Err(ViewerTicketRegistryError::Configuration);
        }
        Ok(Self { capacity, ttl })
    }

    /// Returns the maximum simultaneous unexpired records.
    #[must_use]
    pub const fn capacity(self) -> usize {
        self.capacity
    }

    /// Returns the configured one-time ticket lifetime.
    #[must_use]
    pub const fn ttl(self) -> Duration {
        self.ttl
    }
}

impl Default for ViewerTicketRegistryConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_VIEWER_TICKET_CAPACITY,
            ttl: Duration::from_secs(MAX_VIEWER_TICKET_TTL_SECONDS as u64),
        }
    }
}

/// Clock seam used to make issuance and expiry deterministic in tests.
pub trait ViewerTicketClock: Send + Sync + 'static {
    /// Samples external wall time and internal monotonic time together.
    fn now(&self) -> Result<ViewerTicketClockReading, ViewerTicketRegistryError>;
}

/// One coherent clock sample used for external claims and internal expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerTicketClockReading {
    unix_timestamp_nanos: i128,
    monotonic_elapsed_nanos: u128,
}

impl ViewerTicketClockReading {
    /// Creates a clock sample. Timestamp range validation remains at issuance.
    #[must_use]
    pub const fn new(unix_timestamp_nanos: i128, monotonic_elapsed_nanos: u128) -> Self {
        Self {
            unix_timestamp_nanos,
            monotonic_elapsed_nanos,
        }
    }

    /// Returns the external Unix-epoch timestamp sample.
    #[must_use]
    pub const fn unix_timestamp_nanos(self) -> i128 {
        self.unix_timestamp_nanos
    }

    /// Returns elapsed nanoseconds from the clock's private monotonic origin.
    #[must_use]
    pub const fn monotonic_elapsed_nanos(self) -> u128 {
        self.monotonic_elapsed_nanos
    }
}

/// Entropy seam used for the registry key and each exact 32-byte ticket.
pub trait ViewerTicketEntropy: Send + Sync + 'static {
    /// Fills the complete destination with cryptographically secure bytes.
    fn fill(&self, destination: &mut [u8]) -> Result<(), ViewerTicketRegistryError>;
}

#[derive(Debug)]
struct SystemViewerTicketClock {
    monotonic_origin: Instant,
}

impl SystemViewerTicketClock {
    fn new() -> Self {
        Self {
            monotonic_origin: Instant::now(),
        }
    }
}

impl ViewerTicketClock for SystemViewerTicketClock {
    fn now(&self) -> Result<ViewerTicketClockReading, ViewerTicketRegistryError> {
        // Sample monotonic time first. If this thread is descheduled between
        // clocks, the internal deadline becomes conservatively earlier than
        // the public wall-clock claim, never later.
        let monotonic_elapsed_nanos = self.monotonic_origin.elapsed().as_nanos();
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ViewerTicketRegistryError::Clock)?;
        let unix_timestamp_nanos =
            i128::try_from(elapsed.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
        Ok(ViewerTicketClockReading::new(
            unix_timestamp_nanos,
            monotonic_elapsed_nanos,
        ))
    }
}

#[derive(Debug)]
struct SystemViewerTicketEntropy;

impl ViewerTicketEntropy for SystemViewerTicketEntropy {
    fn fill(&self, destination: &mut [u8]) -> Result<(), ViewerTicketRegistryError> {
        getrandom::fill(destination).map_err(|_| ViewerTicketRegistryError::Entropy)
    }
}

struct ViewerTicketKey([u8; TICKET_BYTES]);

impl fmt::Debug for ViewerTicketKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ViewerTicketKey([REDACTED])")
    }
}

impl Drop for ViewerTicketKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TicketDigest([u8; TICKET_BYTES]);

impl fmt::Debug for TicketDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TicketDigest([REDACTED])")
    }
}

/// Complete non-secret claims retained beside a ticket's keyed digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerTicketClaims {
    /// Principal authorized at issuance.
    pub principal_id: ViewerPrincipalId,
    /// Endpoint class allowed to consume the ticket.
    pub audience: ViewerTicketAudience,
    /// Exact desktop resource.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime.
    pub desktop_generation: DesktopGeneration,
    /// Canonical browser origin.
    pub origin: ViewerOrigin,
    /// Always view-only in release one.
    pub mode: ViewerMode,
    /// Registry clock at issuance.
    pub issued_at: Timestamp,
    /// Strict ticket expiry.
    pub expires_at: Timestamp,
    /// Always single-use in release one.
    pub use_policy: ViewerTicketUsePolicy,
}

impl ViewerTicketClaims {
    fn from_ticket(ticket: &OneTimeViewerTicket) -> Self {
        Self {
            principal_id: ticket.principal_id.clone(),
            audience: ticket.audience,
            desktop_id: ticket.desktop_id,
            desktop_generation: ticket.desktop_generation,
            origin: ticket.origin.clone(),
            mode: ticket.mode,
            issued_at: ticket.issued_at.clone(),
            expires_at: ticket.expires_at.clone(),
            use_policy: ticket.use_policy,
        }
    }

    fn expires_at_nanos(&self) -> Result<i128, ViewerTicketRegistryError> {
        self.expires_at
            .unix_timestamp_nanos()
            .map_err(|_| ViewerTicketRegistryError::Internal)
    }
}

struct StoredViewerTicket {
    claims: ViewerTicketClaims,
    expires_after_monotonic_nanos: u128,
}

/// Audience proof supplied by the future ticket-consuming gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerTicketConsumeAudience {
    /// Exact release-one viewer WebSocket gateway.
    ViewerWebsocket,
    /// Caller has not proved that it is the viewer WebSocket gateway.
    Unrecognized,
}

/// Full expected claim set for one atomic ticket-consumption attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerTicketConsumeRequest {
    /// Plaintext bearer presented only at this authentication boundary.
    pub ticket: ViewerTicketSecret,
    /// Verified consuming endpoint class.
    pub audience: ViewerTicketConsumeAudience,
    /// Exact routed desktop.
    pub desktop_id: DesktopId,
    /// Current desktop lifetime.
    pub desktop_generation: DesktopGeneration,
    /// Canonical upgrade Origin.
    pub origin: ViewerOrigin,
    /// Requested session mode.
    pub mode: ViewerMode,
}

/// HMAC-backed, bounded in-memory one-time ticket registry.
///
/// This component authenticates viewer gateway admission only. It does not
/// start a viewer backend, expose a WebSocket route, or claim viewer availability.
pub struct InMemoryViewerTicketRegistry {
    config: ViewerTicketRegistryConfig,
    key: ViewerTicketKey,
    clock: Arc<dyn ViewerTicketClock>,
    entropy: Arc<dyn ViewerTicketEntropy>,
    records: Mutex<BTreeMap<TicketDigest, StoredViewerTicket>>,
}

impl InMemoryViewerTicketRegistry {
    /// Creates a production registry using the system clock and `getrandom`.
    pub fn new(config: ViewerTicketRegistryConfig) -> Result<Self, ViewerTicketRegistryError> {
        Self::with_sources(
            config,
            Arc::new(SystemViewerTicketClock::new()),
            Arc::new(SystemViewerTicketEntropy),
        )
    }

    /// Creates a registry with explicit deterministic clock and entropy seams.
    pub fn with_sources(
        config: ViewerTicketRegistryConfig,
        clock: Arc<dyn ViewerTicketClock>,
        entropy: Arc<dyn ViewerTicketEntropy>,
    ) -> Result<Self, ViewerTicketRegistryError> {
        ViewerTicketRegistryConfig::new(config.capacity, config.ttl)?;
        let mut key = [0_u8; TICKET_BYTES];
        entropy.fill(&mut key)?;
        Ok(Self {
            config,
            key: ViewerTicketKey(key),
            clock,
            entropy,
            records: Mutex::new(BTreeMap::new()),
        })
    }

    /// Mints and retains one origin/principal/generation-bound ticket.
    pub fn issue_ticket(
        &self,
        context: ViewerTicketIssueContext,
        request: ViewerTicketRequest,
    ) -> Result<OneTimeViewerTicket, ViewerTicketRegistryError> {
        if !context.principal.has_grant(Grant::ViewerRead)
            || request.validate().is_err()
            || request.desktop_id != context.desktop_id
            || request.desktop_generation != context.desktop_generation
            || context.audience != ViewerTicketAudience::ViewerWebsocket
        {
            return Err(ViewerTicketRegistryError::InvalidRequest);
        }
        ViewerOrigin::new(context.origin.as_str())
            .map_err(|_| ViewerTicketRegistryError::InvalidRequest)?;
        let principal_id = ViewerPrincipalId::new(context.principal.id())
            .map_err(|_| ViewerTicketRegistryError::InvalidRequest)?;
        let now = self.clock.now()?;
        let issued_nanos = now.unix_timestamp_nanos();
        let ttl_nanos = i128::try_from(self.config.ttl.as_nanos())
            .map_err(|_| ViewerTicketRegistryError::Configuration)?;
        let expires_nanos = issued_nanos
            .checked_add(ttl_nanos)
            .ok_or(ViewerTicketRegistryError::Clock)?;
        let issued_at = Timestamp::from_unix_timestamp_nanos(issued_nanos)
            .map_err(|_| ViewerTicketRegistryError::Clock)?;
        let expires_at = Timestamp::from_unix_timestamp_nanos(expires_nanos)
            .map_err(|_| ViewerTicketRegistryError::Clock)?;

        let mut random = [0_u8; TICKET_BYTES];
        self.entropy.fill(&mut random)?;
        let secret = ViewerTicketSecret::new(encode_ticket_secret(random))
            .map_err(|_| ViewerTicketRegistryError::Entropy)?;
        random.fill(0);
        let digest = self.digest(secret.expose_secret())?;
        let ticket = OneTimeViewerTicket {
            ticket: secret,
            principal_id,
            audience: context.audience,
            desktop_id: request.desktop_id,
            desktop_generation: request.desktop_generation,
            origin: context.origin,
            mode: request.mode,
            issued_at,
            expires_at,
            use_policy: ViewerTicketUsePolicy::SingleUse,
        };
        ticket
            .validate()
            .map_err(|_| ViewerTicketRegistryError::Internal)?;
        let claims = ViewerTicketClaims::from_ticket(&ticket);
        let expires_after_monotonic_nanos = now
            .monotonic_elapsed_nanos()
            .checked_add(self.config.ttl.as_nanos())
            .ok_or(ViewerTicketRegistryError::Clock)?;
        let mut records = self.lock_records();
        purge_expired(&mut records, now.monotonic_elapsed_nanos());
        if records.len() >= self.config.capacity {
            return Err(ViewerTicketRegistryError::Capacity);
        }
        if records.contains_key(&digest) {
            return Err(ViewerTicketRegistryError::Entropy);
        }
        records.insert(
            digest,
            StoredViewerTicket {
                claims,
                expires_after_monotonic_nanos,
            },
        );
        Ok(ticket)
    }

    /// Atomically consumes the first attempt whose complete claims match.
    ///
    /// Claim mismatches do not remove the ticket. Expired tickets are removed,
    /// and exactly one of concurrent valid attempts can succeed.
    pub fn consume(
        &self,
        request: &ViewerTicketConsumeRequest,
    ) -> Result<ViewerTicketClaims, ViewerTicketRegistryError> {
        let now = self.clock.now()?;
        let digest = self.digest(request.ticket.expose_secret())?;
        let mut records = self.lock_records();
        let target = records
            .get(&digest)
            .map(|record| (record.claims.clone(), record.expires_after_monotonic_nanos));
        if target
            .as_ref()
            .is_some_and(|(claims, _)| claims.expires_at_nanos().is_err())
        {
            records.remove(&digest);
            purge_expired(&mut records, now.monotonic_elapsed_nanos());
            return Err(ViewerTicketRegistryError::Internal);
        }
        if target
            .as_ref()
            .is_some_and(|(_, expires)| *expires <= now.monotonic_elapsed_nanos())
        {
            records.remove(&digest);
            purge_expired(&mut records, now.monotonic_elapsed_nanos());
            return Err(ViewerTicketRegistryError::Expired);
        }
        purge_expired(&mut records, now.monotonic_elapsed_nanos());
        let Some((claims, _)) = target else {
            return Err(ViewerTicketRegistryError::NotFound);
        };
        let audience_matches = matches!(
            (request.audience, claims.audience),
            (
                ViewerTicketConsumeAudience::ViewerWebsocket,
                ViewerTicketAudience::ViewerWebsocket
            )
        );
        if !audience_matches
            || request.desktop_id != claims.desktop_id
            || request.desktop_generation != claims.desktop_generation
            || request.origin != claims.origin
            || request.mode != claims.mode
        {
            return Err(ViewerTicketRegistryError::ClaimMismatch);
        }
        records
            .remove(&digest)
            .map(|record| record.claims)
            .ok_or(ViewerTicketRegistryError::NotFound)
    }

    /// Returns the currently retained record count without exposing claims.
    #[must_use]
    pub fn retained_ticket_count(&self) -> usize {
        self.lock_records().len()
    }

    fn digest(&self, secret: &str) -> Result<TicketDigest, ViewerTicketRegistryError> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key.0)
            .map_err(|_| ViewerTicketRegistryError::Internal)?;
        mac.update(DIGEST_DOMAIN);
        mac.update(secret.as_bytes());
        let bytes = mac.finalize().into_bytes();
        let mut digest = [0_u8; TICKET_BYTES];
        digest.copy_from_slice(&bytes);
        Ok(TicketDigest(digest))
    }

    fn lock_records(&self) -> MutexGuard<'_, BTreeMap<TicketDigest, StoredViewerTicket>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ViewerTicketService for InMemoryViewerTicketRegistry {
    fn issue<'a>(
        &'a self,
        context: ViewerTicketIssueContext,
        request: ViewerTicketRequest,
    ) -> ViewerTicketFuture<'a, Result<OneTimeViewerTicket, ControlPlaneError>> {
        Box::pin(async move {
            self.issue_ticket(context, request)
                .map_err(|error| match error {
                    ViewerTicketRegistryError::Capacity => ControlPlaneError::ResourceExhausted,
                    ViewerTicketRegistryError::InvalidRequest
                    | ViewerTicketRegistryError::ClaimMismatch => ControlPlaneError::InvalidRequest,
                    ViewerTicketRegistryError::NotFound
                    | ViewerTicketRegistryError::Expired
                    | ViewerTicketRegistryError::Configuration
                    | ViewerTicketRegistryError::Entropy
                    | ViewerTicketRegistryError::Clock
                    | ViewerTicketRegistryError::Internal => ControlPlaneError::Internal,
                })
        })
    }

    fn consume_for_gateway<'a>(
        &'a self,
        request: ViewerTicketConsumeRequest,
    ) -> ViewerTicketFuture<'a, Result<ViewerTicketClaims, ControlPlaneError>> {
        Box::pin(async move {
            self.consume(&request).map_err(|error| match error {
                ViewerTicketRegistryError::NotFound
                | ViewerTicketRegistryError::Expired
                | ViewerTicketRegistryError::ClaimMismatch
                | ViewerTicketRegistryError::InvalidRequest => ControlPlaneError::PermissionDenied,
                ViewerTicketRegistryError::Capacity => ControlPlaneError::ResourceExhausted,
                ViewerTicketRegistryError::Configuration
                | ViewerTicketRegistryError::Entropy
                | ViewerTicketRegistryError::Clock
                | ViewerTicketRegistryError::Internal => ControlPlaneError::Internal,
            })
        })
    }
}

impl fmt::Debug for InMemoryViewerTicketRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryViewerTicketRegistry")
            .field("capacity", &self.config.capacity)
            .field("ttl", &self.config.ttl)
            .field("retained_ticket_count", &self.retained_ticket_count())
            .field("key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Safe registry failure categories; none retain or display bearer material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ViewerTicketRegistryError {
    /// Capacity or TTL configuration is outside hard bounds.
    #[error("viewer ticket registry configuration is invalid")]
    Configuration,
    /// Secure entropy was unavailable or repeated an existing ticket digest.
    #[error("viewer ticket entropy is unavailable")]
    Entropy,
    /// Current time was unavailable or outside protocol timestamp bounds.
    #[error("viewer ticket clock is unavailable")]
    Clock,
    /// The bounded live-ticket registry is full.
    #[error("viewer ticket capacity is exhausted")]
    Capacity,
    /// Issuance request or trusted context is inconsistent.
    #[error("viewer ticket request is invalid")]
    InvalidRequest,
    /// No retained ticket matches the presented keyed digest.
    #[error("viewer ticket was not found")]
    NotFound,
    /// The matching ticket expired and was removed.
    #[error("viewer ticket expired")]
    Expired,
    /// One or more expected claims did not match; the ticket remains live.
    #[error("viewer ticket claims do not match")]
    ClaimMismatch,
    /// A private registry invariant failed.
    #[error("viewer ticket registry failed safely")]
    Internal,
}

fn purge_expired(
    records: &mut BTreeMap<TicketDigest, StoredViewerTicket>,
    monotonic_elapsed_nanos: u128,
) {
    records.retain(|_, record| {
        record.expires_after_monotonic_nanos > monotonic_elapsed_nanos
            && record.claims.expires_at_nanos().is_ok()
    });
}

fn encode_ticket_secret(bytes: [u8; TICKET_BYTES]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(43);
    for chunk in bytes.chunks_exact(3) {
        encoded.push(char::from(ALPHABET[usize::from(chunk[0] >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((chunk[0] & 0x03) << 4) | (chunk[1] >> 4))],
        ));
        encoded.push(char::from(
            ALPHABET[usize::from(((chunk[1] & 0x0f) << 2) | (chunk[2] >> 6))],
        ));
        encoded.push(char::from(ALPHABET[usize::from(chunk[2] & 0x3f)]));
    }
    let remainder = bytes.chunks_exact(3).remainder();
    if let [first, second] = remainder {
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        encoded.push(char::from(ALPHABET[usize::from((second & 0x0f) << 2)]));
    }
    encoded
}

#[derive(Clone)]
struct ViewerTicketServiceState(SharedViewerTicketService);

pub(crate) fn routes(service: SharedViewerTicketService) -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/desktops/{desktop_id}/viewer-tickets",
            post(issue_ticket),
        )
        .layer(Extension(ViewerTicketServiceState(service)))
}

async fn issue_ticket(
    State(state): State<ApiState>,
    Extension(service): Extension<ViewerTicketServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    headers: HeaderMap,
    body: Result<Json<ViewerTicketRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::ViewerRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Some(origin) = required_allowed_origin(&state, &headers) else {
        return ApiProblem::origin_denied(request_id).into_response();
    };
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if request.validate().is_err() || request.desktop_id != desktop_id {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = crate::control::validate_generation(
        &state,
        desktop_id,
        request.desktop_generation,
        request_id,
    ) {
        return problem.into_response();
    }
    let principal_id = match ViewerPrincipalId::new(principal.id()) {
        Ok(principal_id) => principal_id,
        Err(_) => return ApiProblem::internal(request_id).into_response(),
    };
    let context = ViewerTicketIssueContext::new(
        principal,
        request_id,
        desktop_id,
        request.desktop_generation,
        origin.clone(),
    );
    match service.0.issue(context, request.clone()).await {
        Ok(ticket)
            if ticket.validate().is_ok()
                && ticket.principal_id == principal_id
                && ticket.audience == ViewerTicketAudience::ViewerWebsocket
                && ticket.desktop_id == desktop_id
                && ticket.desktop_generation == request.desktop_generation
                && ticket.origin == origin
                && ticket.mode == request.mode
                && ticket.use_policy == ViewerTicketUsePolicy::SingleUse =>
        {
            ticket_response(ticket)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn required_allowed_origin(state: &ApiState, headers: &HeaderMap) -> Option<ViewerOrigin> {
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

fn ticket_response(ticket: OneTimeViewerTicket) -> Response {
    let mut response = (StatusCode::CREATED, Json(ticket)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Barrier,
            atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
    };

    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        AllowedOrigins, ApiServices, Authentication, DesktopReadiness, ReadinessHandle,
        ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
        api_router, api_router_with_services, control::UnavailableControlPlane,
        observation::UnavailableObservationPlane,
    };

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
    const ORIGIN: &str = "https://viewer.example";
    const NOW_NANOS: i64 = 1_784_592_000_000_000_000;

    struct TestClock {
        unix_timestamp_nanos: AtomicI64,
        monotonic_nanos: AtomicU64,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                unix_timestamp_nanos: AtomicI64::new(NOW_NANOS),
                monotonic_nanos: AtomicU64::new(0),
            }
        }

        fn advance(&self, duration: Duration) -> Result<(), ViewerTicketRegistryError> {
            let nanos =
                i64::try_from(duration.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
            let monotonic_nanos =
                u64::try_from(duration.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
            self.unix_timestamp_nanos.fetch_add(nanos, Ordering::SeqCst);
            self.monotonic_nanos
                .fetch_add(monotonic_nanos, Ordering::SeqCst);
            Ok(())
        }

        fn rewind_wall_clock(&self, duration: Duration) -> Result<(), ViewerTicketRegistryError> {
            let nanos =
                i64::try_from(duration.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
            self.unix_timestamp_nanos.fetch_sub(nanos, Ordering::SeqCst);
            Ok(())
        }

        fn advance_wall_clock(&self, duration: Duration) -> Result<(), ViewerTicketRegistryError> {
            let nanos =
                i64::try_from(duration.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
            self.unix_timestamp_nanos.fetch_add(nanos, Ordering::SeqCst);
            Ok(())
        }

        fn advance_monotonic(&self, duration: Duration) -> Result<(), ViewerTicketRegistryError> {
            let nanos =
                u64::try_from(duration.as_nanos()).map_err(|_| ViewerTicketRegistryError::Clock)?;
            self.monotonic_nanos.fetch_add(nanos, Ordering::SeqCst);
            Ok(())
        }
    }

    impl ViewerTicketClock for TestClock {
        fn now(&self) -> Result<ViewerTicketClockReading, ViewerTicketRegistryError> {
            Ok(ViewerTicketClockReading::new(
                i128::from(self.unix_timestamp_nanos.load(Ordering::SeqCst)),
                u128::from(self.monotonic_nanos.load(Ordering::SeqCst)),
            ))
        }
    }

    struct TestEntropy(Mutex<VecDeque<[u8; TICKET_BYTES]>>);

    impl TestEntropy {
        fn new(values: impl IntoIterator<Item = [u8; TICKET_BYTES]>) -> Self {
            Self(Mutex::new(values.into_iter().collect()))
        }
    }

    impl ViewerTicketEntropy for TestEntropy {
        fn fill(&self, destination: &mut [u8]) -> Result<(), ViewerTicketRegistryError> {
            if destination.len() != TICKET_BYTES {
                return Err(ViewerTicketRegistryError::Entropy);
            }
            let Some(value) = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
            else {
                return Err(ViewerTicketRegistryError::Entropy);
            };
            destination.copy_from_slice(&value);
            Ok(())
        }
    }

    fn test_registry(
        capacity: usize,
        ttl: Duration,
        clock: Arc<TestClock>,
        ticket_count: u8,
    ) -> Result<Arc<InMemoryViewerTicketRegistry>, ViewerTicketRegistryError> {
        let values = (0..=ticket_count).map(|value| [value; TICKET_BYTES]);
        Ok(Arc::new(InMemoryViewerTicketRegistry::with_sources(
            ViewerTicketRegistryConfig::new(capacity, ttl)?,
            clock,
            Arc::new(TestEntropy::new(values)),
        )?))
    }

    fn issue_context(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
    ) -> Result<ViewerTicketIssueContext, Box<dyn std::error::Error>> {
        Ok(ViewerTicketIssueContext::new(
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            RequestId::new(),
            desktop_id,
            generation,
            ViewerOrigin::new(ORIGIN)?,
        ))
    }

    fn issue_request(desktop_id: DesktopId, generation: DesktopGeneration) -> ViewerTicketRequest {
        ViewerTicketRequest {
            desktop_id,
            desktop_generation: generation,
            mode: ViewerMode::ViewOnly,
        }
    }

    fn consume_request(ticket: &OneTimeViewerTicket) -> ViewerTicketConsumeRequest {
        ViewerTicketConsumeRequest {
            ticket: ticket.ticket.clone(),
            audience: ViewerTicketConsumeAudience::ViewerWebsocket,
            desktop_id: ticket.desktop_id,
            desktop_generation: ticket.desktop_generation,
            origin: ticket.origin.clone(),
            mode: ticket.mode,
        }
    }

    #[test]
    fn thirty_two_bytes_encode_as_unpadded_base64url() {
        let secret = encode_ticket_secret([0xff; 32]);
        assert_eq!(secret.len(), 43);
        assert!(
            secret
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert!(!secret.contains('='));
    }

    #[test]
    fn capacity_is_bounded_and_expired_records_are_reclaimed()
    -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(1, Duration::from_secs(60), Arc::clone(&clock), 3)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        assert_eq!(
            registry.issue_ticket(
                issue_context(desktop_id, generation)?,
                issue_request(desktop_id, generation),
            ),
            Err(ViewerTicketRegistryError::Capacity)
        );
        clock.advance(Duration::from_secs(61))?;
        assert!(
            registry
                .issue_ticket(
                    issue_context(desktop_id, generation)?,
                    issue_request(desktop_id, generation),
                )
                .is_ok()
        );
        assert_eq!(registry.retained_ticket_count(), 1);
        Ok(())
    }

    #[test]
    fn expiry_removes_the_matching_ticket() -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(1), Arc::clone(&clock), 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        clock.advance(Duration::from_secs(2))?;
        assert_eq!(
            registry.consume(&consume_request(&ticket)),
            Err(ViewerTicketRegistryError::Expired)
        );
        assert_eq!(registry.retained_ticket_count(), 0);
        Ok(())
    }

    #[test]
    fn ticket_expires_at_exactly_one_monotonic_ttl_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        clock.advance_monotonic(Duration::from_secs(17))?;
        let registry = test_registry(2, Duration::from_secs(5), Arc::clone(&clock), 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;

        clock.advance_monotonic(Duration::from_secs(5))?;

        assert_eq!(
            registry.consume(&consume_request(&ticket)),
            Err(ViewerTicketRegistryError::Expired)
        );
        assert_eq!(registry.retained_ticket_count(), 0);
        Ok(())
    }

    #[test]
    fn wall_clock_rollback_cannot_extend_ticket_lifetime() -> Result<(), Box<dyn std::error::Error>>
    {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(1), Arc::clone(&clock), 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;

        clock.rewind_wall_clock(Duration::from_secs(3_600))?;
        clock.advance_monotonic(Duration::from_secs(2))?;

        assert_eq!(
            registry.consume(&consume_request(&ticket)),
            Err(ViewerTicketRegistryError::Expired)
        );
        assert_eq!(registry.retained_ticket_count(), 0);
        Ok(())
    }

    #[test]
    fn wall_clock_jump_cannot_prematurely_expire_ticket() -> Result<(), Box<dyn std::error::Error>>
    {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), Arc::clone(&clock), 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;

        clock.advance_wall_clock(Duration::from_secs(3_600))?;

        assert_eq!(
            registry.consume(&consume_request(&ticket))?.principal_id,
            ticket.principal_id
        );
        Ok(())
    }

    #[test]
    fn invalid_binding_claims_do_not_burn_a_valid_ticket() -> Result<(), Box<dyn std::error::Error>>
    {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), clock, 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        let valid = consume_request(&ticket);

        let mut wrong_origin = valid.clone();
        wrong_origin.origin = ViewerOrigin::new("https://other.example")?;
        assert_eq!(
            registry.consume(&wrong_origin),
            Err(ViewerTicketRegistryError::ClaimMismatch)
        );
        let mut wrong_generation = valid.clone();
        wrong_generation.desktop_generation = DesktopGeneration::new();
        assert_eq!(
            registry.consume(&wrong_generation),
            Err(ViewerTicketRegistryError::ClaimMismatch)
        );
        let mut wrong_audience = valid.clone();
        wrong_audience.audience = ViewerTicketConsumeAudience::Unrecognized;
        assert_eq!(
            registry.consume(&wrong_audience),
            Err(ViewerTicketRegistryError::ClaimMismatch)
        );
        assert_eq!(registry.retained_ticket_count(), 1);
        assert_eq!(registry.consume(&valid)?.principal_id, ticket.principal_id);
        assert_eq!(registry.retained_ticket_count(), 0);
        Ok(())
    }

    #[test]
    fn ticket_secret_authenticates_the_stored_principal_claim()
    -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), clock, 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;

        let claims = registry.consume(&consume_request(&ticket))?;

        assert_eq!(claims.principal_id, ticket.principal_id);
        assert_eq!(claims.desktop_id, desktop_id);
        assert_eq!(claims.desktop_generation, generation);
        Ok(())
    }

    #[test]
    fn concurrent_valid_attempts_consume_exactly_once() -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), clock, 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        let request = consume_request(&ticket);
        let barrier = Arc::new(Barrier::new(3));
        let first_registry = Arc::clone(&registry);
        let first_request = request.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = thread::spawn(move || {
            first_barrier.wait();
            first_registry.consume(&first_request)
        });
        let second_registry = Arc::clone(&registry);
        let second_barrier = Arc::clone(&barrier);
        let second = thread::spawn(move || {
            second_barrier.wait();
            second_registry.consume(&request)
        });
        barrier.wait();
        let first = first
            .join()
            .map_err(|_| std::io::Error::other("first consumer panicked"))?;
        let second = second
            .join()
            .map_err(|_| std::io::Error::other("second consumer panicked"))?;
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(
            usize::from(matches!(first, Err(ViewerTicketRegistryError::NotFound)))
                + usize::from(matches!(second, Err(ViewerTicketRegistryError::NotFound))),
            1
        );
        Ok(())
    }

    #[test]
    fn debug_and_errors_never_expose_ticket_material() -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), clock, 1)?;
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let ticket = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        let secret = ticket.ticket.expose_secret();
        assert!(!format!("{registry:?}").contains(secret));
        assert!(!format!("{:?}", consume_request(&ticket)).contains(secret));
        assert!(!format!("{:?}", ViewerTicketRegistryError::NotFound).contains(secret));
        Ok(())
    }

    struct CountingViewerService {
        calls: AtomicUsize,
        ticket: Mutex<Option<OneTimeViewerTicket>>,
    }

    impl CountingViewerService {
        fn unavailable() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                ticket: Mutex::new(None),
            }
        }

        fn with_ticket(ticket: OneTimeViewerTicket) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                ticket: Mutex::new(Some(ticket)),
            }
        }
    }

    impl ViewerTicketService for CountingViewerService {
        fn issue<'a>(
            &'a self,
            _: ViewerTicketIssueContext,
            _: ViewerTicketRequest,
        ) -> ViewerTicketFuture<'a, Result<OneTimeViewerTicket, ControlPlaneError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let ticket = self
                .ticket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Box::pin(async move { ticket.ok_or(ControlPlaneError::CapabilityUnavailable) })
        }
    }

    fn application(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        principal: Principal,
        service: Arc<dyn ViewerTicketService>,
    ) -> Result<Router, Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider = StaticTokenProvider::single(TOKEN, principal)?;
        Ok(api_router_with_services(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::exact([ORIGIN.to_owned()])?,
            ApiServices::new(
                Arc::new(UnavailableControlPlane),
                Arc::new(UnavailableObservationPlane),
            )
            .with_viewer_ticket_service(service),
        ))
    }

    fn authorized(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(
            header::AUTHORIZATION,
            "Bearer 0123456789abcdef0123456789abcdef",
        )
    }

    fn http_request(
        desktop_id: DesktopId,
        request: &ViewerTicketRequest,
    ) -> Result<Request<Body>, Box<dyn std::error::Error>> {
        Ok(authorized(Request::post(format!(
            "/v1/desktops/{desktop_id}/viewer-tickets"
        )))
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(request)?))?)
    }

    #[tokio::test]
    async fn auth_grant_origin_scope_and_generation_fail_before_service_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let request = issue_request(desktop_id, generation);

        let service = Arc::new(CountingViewerService::unavailable());
        let missing_auth = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(
            Request::post(format!("/v1/desktops/{desktop_id}/viewer-tickets"))
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&request)?))?,
        )
        .await?;
        assert_eq!(missing_auth.status(), StatusCode::UNAUTHORIZED);

        let missing_grant = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::DesktopStatus])?,
            service.clone(),
        )?
        .oneshot(http_request(desktop_id, &request)?)
        .await?;
        assert_eq!(missing_grant.status(), StatusCode::FORBIDDEN);

        let missing_origin = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(
            authorized(Request::post(format!(
                "/v1/desktops/{desktop_id}/viewer-tickets"
            )))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&request)?))?,
        )
        .await?;
        assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);

        let disallowed_origin = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(
            authorized(Request::post(format!(
                "/v1/desktops/{desktop_id}/viewer-tickets"
            )))
            .header(header::ORIGIN, "https://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&request)?))?,
        )
        .await?;
        assert_eq!(disallowed_origin.status(), StatusCode::FORBIDDEN);

        let mut strict_body = serde_json::to_value(&request)?;
        strict_body["unexpected"] = serde_json::json!(true);
        let strict = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(
            authorized(Request::post(format!(
                "/v1/desktops/{desktop_id}/viewer-tickets"
            )))
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&strict_body)?))?,
        )
        .await?;
        assert_eq!(strict.status(), StatusCode::BAD_REQUEST);

        let mismatched_scope = issue_request(DesktopId::new(), generation);
        let scope = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(http_request(desktop_id, &mismatched_scope)?)
        .await?;
        assert_eq!(scope.status(), StatusCode::BAD_REQUEST);

        let stale = issue_request(desktop_id, DesktopGeneration::new());
        let stale_response = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            service.clone(),
        )?
        .oneshot(http_request(desktop_id, &stale)?)
        .await?;
        assert_eq!(stale_response.status(), StatusCode::CONFLICT);
        assert_eq!(service.calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn registry_ticket_response_is_valid_bound_and_never_cached()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(4, Duration::from_secs(60), clock, 1)?;
        let response = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            registry.clone(),
        )?
        .oneshot(http_request(
            desktop_id,
            &issue_request(desktop_id, generation),
        )?)
        .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE))
        );
        let body = axum::body::to_bytes(response.into_body(), 16 * 1_024).await?;
        let ticket: OneTimeViewerTicket = serde_json::from_slice(&body)?;
        assert!(ticket.validate().is_ok());
        assert_eq!(ticket.principal_id.as_str(), "viewer-principal");
        assert_eq!(ticket.origin.as_str(), ORIGIN);
        assert_eq!(ticket.desktop_id, desktop_id);
        assert_eq!(ticket.desktop_generation, generation);
        assert_eq!(registry.retained_ticket_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_service_ticket_is_rejected_and_default_service_is_unavailable()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let clock = Arc::new(TestClock::new());
        let registry = test_registry(2, Duration::from_secs(60), clock, 1)?;
        let mut invalid = registry.issue_ticket(
            issue_context(desktop_id, generation)?,
            issue_request(desktop_id, generation),
        )?;
        invalid.origin = ViewerOrigin::new("https://other.example")?;
        let fake = Arc::new(CountingViewerService::with_ticket(invalid));
        let invalid_response = application(
            desktop_id,
            generation,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
            fake,
        )?
        .oneshot(http_request(
            desktop_id,
            &issue_request(desktop_id, generation),
        )?)
        .await?;
        assert_eq!(invalid_response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider = StaticTokenProvider::single(
            TOKEN,
            Principal::new("viewer-principal", [Grant::ViewerRead])?,
        )?;
        let unavailable = api_router(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::exact([ORIGIN.to_owned()])?,
        )
        .oneshot(http_request(
            desktop_id,
            &issue_request(desktop_id, generation),
        )?)
        .await?;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }
}
