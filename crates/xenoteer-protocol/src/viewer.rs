//! Short-lived, origin-bound, view-only viewer ticket contracts.

use core::fmt;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{DesktopGeneration, DesktopId, Timestamp};

/// Release-one viewer tickets expire no later than sixty seconds after issue.
pub const MAX_VIEWER_TICKET_TTL_SECONDS: i128 = 60;
/// Maximum canonical browser-origin bytes.
pub const MAX_VIEWER_ORIGIN_BYTES: usize = 512;
/// Minimum URL-safe random ticket characters for 256-bit material.
pub const MIN_VIEWER_TICKET_BYTES: usize = 43;
/// Defensive ticket representation ceiling.
pub const MAX_VIEWER_TICKET_BYTES: usize = 128;
/// Maximum authenticated principal identifier bytes copied into ticket claims.
pub const MAX_VIEWER_PRINCIPAL_ID_BYTES: usize = 128;

/// Exact HTTP(S) browser origin to which a viewer ticket is bound.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "viewer_origin_schema")]
pub struct ViewerOrigin(String);

fn viewer_origin_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 8,
        "maxLength": MAX_VIEWER_ORIGIN_BYTES,
        "pattern": "^https?://[^/?#@\\s]+$"
    })
}

impl ViewerOrigin {
    /// Creates a canonical pathless, queryless, fragmentless, credential-free origin.
    ///
    /// DNS names must be lowercase without a trailing dot, IP literals must use
    /// their canonical text form, and default or non-canonical ports are rejected.
    /// Deployment/server integration must compare the resulting exact value
    /// against its configured allowlist.
    pub fn new(value: impl Into<String>) -> Result<Self, ViewerValidationError> {
        let value = value.into();
        if value.len() > MAX_VIEWER_ORIGIN_BYTES
            || value
                .bytes()
                .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'%' | b'\\'))
        {
            return Err(ViewerValidationError::Origin);
        }
        let expected_scheme = if value.starts_with("https://") {
            "https"
        } else if value.starts_with("http://") {
            "http"
        } else {
            return Err(ViewerValidationError::Origin);
        };
        let uri = http::Uri::from_str(&value).map_err(|_| ViewerValidationError::Origin)?;
        let scheme = uri.scheme_str().ok_or(ViewerValidationError::Origin)?;
        let authority = uri.authority().ok_or(ViewerValidationError::Origin)?;
        if scheme != expected_scheme
            || authority.as_str().contains('@')
            || value != format!("{scheme}://{authority}")
        {
            return Err(ViewerValidationError::Origin);
        }
        validate_canonical_authority(scheme, authority.as_str())?;
        Ok(Self(value))
    }

    /// Returns the exact validated origin.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ViewerOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ViewerOrigin")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ViewerOrigin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ViewerOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

fn validate_canonical_authority(
    scheme: &str,
    authority: &str,
) -> Result<(), ViewerValidationError> {
    let (host, port) = if authority.starts_with('[') {
        let closing = authority.find(']').ok_or(ViewerValidationError::Origin)?;
        let bracketed_host = &authority[..=closing];
        let literal = &authority[1..closing];
        let address = Ipv6Addr::from_str(literal).map_err(|_| ViewerValidationError::Origin)?;
        if bracketed_host != format!("[{address}]") {
            return Err(ViewerValidationError::Origin);
        }
        let remainder = authority
            .get(closing + 1..)
            .ok_or(ViewerValidationError::Origin)?;
        let port = if remainder.is_empty() {
            None
        } else {
            Some(
                remainder
                    .strip_prefix(':')
                    .ok_or(ViewerValidationError::Origin)?,
            )
        };
        (bracketed_host, port)
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        validate_canonical_reg_name_or_ipv4(host)?;
        (host, port)
    };
    if host.is_empty() {
        return Err(ViewerValidationError::Origin);
    }

    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ViewerValidationError::Origin);
        }
        let parsed = port
            .parse::<u16>()
            .map_err(|_| ViewerValidationError::Origin)?;
        if parsed == 0
            || port != parsed.to_string()
            || matches!((scheme, parsed), ("http", 80) | ("https", 443))
        {
            return Err(ViewerValidationError::Origin);
        }
    }
    Ok(())
}

fn validate_canonical_reg_name_or_ipv4(host: &str) -> Result<(), ViewerValidationError> {
    if host.is_empty() || host.len() > 253 || host.ends_with('.') {
        return Err(ViewerValidationError::Origin);
    }
    if let Ok(address) = Ipv4Addr::from_str(host) {
        return if address.to_string() == host {
            Ok(())
        } else {
            Err(ViewerValidationError::Origin)
        };
    }
    let final_label = host.rsplit('.').next().unwrap_or(host);
    let numeric_final_label = final_label.bytes().all(|byte| byte.is_ascii_digit())
        || final_label.strip_prefix("0x").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if numeric_final_label {
        return Err(ViewerValidationError::Origin);
    }
    if host.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || label.starts_with('-')
            || label.ends_with('-')
    }) {
        return Err(ViewerValidationError::Origin);
    }
    Ok(())
}

/// Stable authenticated identity copied into a viewer ticket claim.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "viewer_principal_id_schema")]
pub struct ViewerPrincipalId(String);

fn viewer_principal_id_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_VIEWER_PRINCIPAL_ID_BYTES,
        "pattern": "^[A-Za-z0-9._:@-]+$"
    })
}

impl ViewerPrincipalId {
    /// Creates a bounded identifier using the server authentication vocabulary.
    pub fn new(value: impl Into<String>) -> Result<Self, ViewerValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_VIEWER_PRINCIPAL_ID_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
            })
        {
            return Err(ViewerValidationError::Principal);
        }
        Ok(Self(value))
    }

    /// Returns the stable authenticated principal identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ViewerPrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ViewerPrincipalId")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ViewerPrincipalId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ViewerPrincipalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Random single-use bearer material distinct from long-lived API authority.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
#[schemars(schema_with = "viewer_ticket_schema")]
pub struct ViewerTicketSecret(String);

fn viewer_ticket_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": MIN_VIEWER_TICKET_BYTES,
        "maxLength": MAX_VIEWER_TICKET_BYTES,
        "pattern": "^[A-Za-z0-9_-]+$",
        "readOnly": true
    })
}

impl ViewerTicketSecret {
    /// Creates checked unpadded base64url-style ticket material.
    pub fn new(value: impl Into<String>) -> Result<Self, ViewerValidationError> {
        let value = value.into();
        if !(MIN_VIEWER_TICKET_BYTES..=MAX_VIEWER_TICKET_BYTES).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ViewerValidationError::Ticket);
        }
        Ok(Self(value))
    }

    /// Returns secret ticket material only at the viewer bootstrap boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ViewerTicketSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ViewerTicketSecret([REDACTED])")
    }
}

impl Serialize for ViewerTicketSecret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ViewerTicketSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Closed release-one viewer control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewerMode {
    /// Human observation only; key, pointer, resize, and clipboard are disabled.
    ViewOnly,
}

/// Closed service audience for viewer bearer material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewerTicketAudience {
    /// Browser viewer WebSocket upgrade endpoint only.
    ViewerWebsocket,
}

/// Explicit ticket issuance request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewerTicketRequest {
    /// Desktop resource to observe.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime; tickets never cross generations.
    pub desktop_generation: DesktopGeneration,
    /// Explicit future-safe mode; release one accepts only view-only.
    pub mode: ViewerMode,
}

impl ViewerTicketRequest {
    /// Revalidates public desktop identity. The server binds the authenticated
    /// request's canonical Origin header; callers cannot choose another origin
    /// in this body.
    pub fn validate(&self) -> Result<(), ViewerValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(ViewerValidationError::NilIdentifier);
        }
        Ok(())
    }
}

/// Single-use ticket response. Debug output always redacts bearer material.
///
/// The issuer must generate the ticket with a CSPRNG and persist only a keyed
/// digest of its secret. The server-side record must retain the complete claim
/// set (`principal_id`, `audience`, `desktop_id`, `desktop_generation`, `origin`,
/// `mode`, and expiry) plus a consumed flag. A viewer upgrade must atomically
/// look up and consume that record while checking the current time and every
/// claim; wire-shape validation alone is not authorization.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OneTimeViewerTicket {
    /// Secret returned once and stored only as a keyed digest by the server.
    pub ticket: ViewerTicketSecret,
    /// Authenticated principal for whom the server minted this ticket.
    pub principal_id: ViewerPrincipalId,
    /// Exact endpoint class allowed to consume the bearer.
    pub audience: ViewerTicketAudience,
    /// Desktop resource authorized for viewing.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime authorized for viewing.
    pub desktop_generation: DesktopGeneration,
    /// Origin required at the browser WebSocket upgrade.
    pub origin: ViewerOrigin,
    /// Always view-only in release one.
    pub mode: ViewerMode,
    /// Ticket creation instant.
    pub issued_at: Timestamp,
    /// Strictly later expiry, no more than sixty seconds after issue.
    pub expires_at: Timestamp,
    /// Explicit one-time consumption contract.
    pub use_policy: ViewerTicketUsePolicy,
}

impl OneTimeViewerTicket {
    /// Validates scope, secret syntax, and bounded one-time lifetime.
    pub fn validate(&self) -> Result<(), ViewerValidationError> {
        ViewerTicketRequest {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            mode: self.mode,
        }
        .validate()?;
        ViewerPrincipalId::new(self.principal_id.as_str())?;
        ViewerOrigin::new(self.origin.as_str())?;
        ViewerTicketSecret::new(self.ticket.expose_secret())?;
        let issued = self
            .issued_at
            .unix_timestamp_nanos()
            .map_err(|_| ViewerValidationError::Lifetime)?;
        let expires = self
            .expires_at
            .unix_timestamp_nanos()
            .map_err(|_| ViewerValidationError::Lifetime)?;
        let maximum_ns = MAX_VIEWER_TICKET_TTL_SECONDS * 1_000_000_000;
        if expires <= issued || expires - issued > maximum_ns {
            return Err(ViewerValidationError::Lifetime);
        }
        Ok(())
    }
}

impl fmt::Debug for OneTimeViewerTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OneTimeViewerTicket")
            .field("ticket", &"[REDACTED]")
            .field("principal_id", &self.principal_id)
            .field("audience", &self.audience)
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .field("origin", &self.origin)
            .field("mode", &self.mode)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("use_policy", &self.use_policy)
            .finish()
    }
}

/// Closed ticket-consumption policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewerTicketUsePolicy {
    /// Atomically consumed by the first valid viewer WebSocket upgrade.
    SingleUse,
}

/// Replaceable viewer backend availability exposed without socket details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewerBackendState {
    /// Backend passed its bounded RFB/WebSocket probe.
    Available,
    /// Backend is restarting or has exhausted its restart budget.
    Degraded,
    /// Operator policy disabled viewing.
    Disabled,
    /// Required backend is unavailable.
    Unavailable,
}

/// Why an authenticated viewer session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewerSessionEndReason {
    /// Browser closed normally.
    ClientClosed,
    /// Upstream RFB adapter disappeared.
    BackendUnavailable,
    /// Desktop generation changed and stale pixels must not remain live.
    GenerationChanged,
    /// Origin/ticket/session policy failed after establishment.
    PolicyRevoked,
    /// Server began bounded shutdown.
    ServerDraining,
}

/// Content-free lifecycle evidence for an authenticated view-only session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ViewerSessionEvidence {
    /// Authenticated principal whose ticket established this session.
    pub principal_id: ViewerPrincipalId,
    /// Endpoint audience authenticated at consumption.
    pub audience: ViewerTicketAudience,
    /// Exact desktop resource.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime.
    pub desktop_generation: DesktopGeneration,
    /// Origin authenticated for the session.
    pub origin: ViewerOrigin,
    /// Always view-only in release one.
    pub mode: ViewerMode,
    /// Whether the one-time ticket was atomically consumed.
    pub ticket_consumed: bool,
    /// Backend state at session establishment/end.
    pub backend_state: ViewerBackendState,
    /// Session establishment instant.
    pub established_at: Timestamp,
    /// Terminal instant, absent while active.
    pub ended_at: Option<Timestamp>,
    /// Terminal reason, present exactly when `ended_at` is present.
    pub end_reason: Option<ViewerSessionEndReason>,
}

impl ViewerSessionEvidence {
    /// Rejects nil scope, unconsumed sessions, and contradictory terminal fields.
    pub fn validate(&self) -> Result<(), ViewerValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(ViewerValidationError::NilIdentifier);
        }
        ViewerPrincipalId::new(self.principal_id.as_str())?;
        ViewerOrigin::new(self.origin.as_str())?;
        if !self.ticket_consumed || self.ended_at.is_some() != self.end_reason.is_some() {
            return Err(ViewerValidationError::SessionEvidence);
        }
        if let Some(ended_at) = &self.ended_at
            && ended_at < &self.established_at
        {
            return Err(ViewerValidationError::SessionEvidence);
        }
        Ok(())
    }
}

/// Invalid viewer ticket or lifecycle metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ViewerValidationError {
    /// Principal identity is empty, excessive, or outside auth syntax.
    #[error("viewer principal identifier is invalid")]
    Principal,
    /// Desktop identity is nil.
    #[error("viewer scope contains a nil identifier")]
    NilIdentifier,
    /// Origin is not a bounded pathless HTTP(S) origin.
    #[error("viewer origin is invalid")]
    Origin,
    /// Ticket is not bounded unpadded URL-safe bearer material.
    #[error("viewer ticket material is invalid")]
    Ticket,
    /// Ticket expiry is non-positive or exceeds sixty seconds.
    #[error("viewer ticket lifetime is invalid")]
    Lifetime,
    /// Session terminal/consumption evidence is contradictory.
    #[error("viewer session evidence is invalid")]
    SessionEvidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Result<ViewerTicketSecret, ViewerValidationError> {
        ViewerTicketSecret::new("A".repeat(MIN_VIEWER_TICKET_BYTES))
    }

    fn ticket(expires: &str) -> Result<OneTimeViewerTicket, Box<dyn std::error::Error>> {
        Ok(OneTimeViewerTicket {
            ticket: secret()?,
            principal_id: ViewerPrincipalId::new("automation:alice")?,
            audience: ViewerTicketAudience::ViewerWebsocket,
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            origin: ViewerOrigin::new("https://viewer.example")?,
            mode: ViewerMode::ViewOnly,
            issued_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
            expires_at: Timestamp::parse(expires)?,
            use_policy: ViewerTicketUsePolicy::SingleUse,
        })
    }

    #[test]
    fn origin_excludes_paths_credentials_queries_and_fragments() {
        assert!(ViewerOrigin::new("https://viewer.example:8443").is_ok());
        assert!(ViewerOrigin::new("http://127.0.0.1:8080").is_ok());
        assert!(ViewerOrigin::new("https://[2001:db8::1]:8443").is_ok());
        for invalid in [
            "https://viewer.example/path",
            "https://user@viewer.example",
            "https://viewer.example?ticket=x",
            "https://viewer.example#ticket",
            "HTTPS://viewer.example",
            "https://Viewer.example",
            "https://viewer.example.",
            "https://viewer.example:443",
            "http://viewer.example:80",
            "https://viewer.example:08443",
            "https://viewer.example:0",
            "https://viewer.example:65536",
            "https://viewer%2eexample",
            "https://viewer.example\\path",
            "https://-viewer.example",
            "https://viewer..example",
            "http://127.1",
            "http://0x7f000001",
            "http://127.0.0.0x1",
            "https://[2001:0db8::1]",
            "http://:",
            "https://[malformed",
            "file://viewer.example",
        ] {
            assert_eq!(
                ViewerOrigin::new(invalid),
                Err(ViewerValidationError::Origin)
            );
        }
    }

    #[test]
    fn ticket_is_serializable_once_but_never_in_debug() -> Result<(), Box<dyn std::error::Error>> {
        let value = ticket("2026-07-21T00:01:00Z")?;
        let secret = value.ticket.expose_secret();
        assert!(serde_json::to_string(&value)?.contains(secret));
        assert!(!format!("{value:?}").contains(secret));
        assert_eq!(value.principal_id.as_str(), "automation:alice");
        assert_eq!(value.audience, ViewerTicketAudience::ViewerWebsocket);
        assert!(value.validate().is_ok());
        Ok(())
    }

    #[test]
    fn principal_claim_uses_the_authentication_identifier_vocabulary() {
        assert!(ViewerPrincipalId::new("automation:alice@example.test").is_ok());
        assert_eq!(
            ViewerPrincipalId::new("alice smith"),
            Err(ViewerValidationError::Principal)
        );
        assert_eq!(
            ViewerPrincipalId::new("x".repeat(MAX_VIEWER_PRINCIPAL_ID_BYTES + 1)),
            Err(ViewerValidationError::Principal)
        );
    }

    #[test]
    fn ticket_lifetime_cannot_exceed_sixty_seconds() -> Result<(), Box<dyn std::error::Error>> {
        assert!(ticket("2026-07-21T00:01:00Z")?.validate().is_ok());
        assert_eq!(
            ticket("2026-07-21T00:01:00.000000001Z")?.validate(),
            Err(ViewerValidationError::Lifetime)
        );
        Ok(())
    }

    #[test]
    fn terminal_session_fields_move_together() -> Result<(), Box<dyn std::error::Error>> {
        let evidence = ViewerSessionEvidence {
            principal_id: ViewerPrincipalId::new("automation:alice")?,
            audience: ViewerTicketAudience::ViewerWebsocket,
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            origin: ViewerOrigin::new("http://127.0.0.1:8080")?,
            mode: ViewerMode::ViewOnly,
            ticket_consumed: true,
            backend_state: ViewerBackendState::Available,
            established_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
            ended_at: None,
            end_reason: Some(ViewerSessionEndReason::ClientClosed),
        };
        assert_eq!(
            evidence.validate(),
            Err(ViewerValidationError::SessionEvidence)
        );
        Ok(())
    }
}
