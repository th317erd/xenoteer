//! Strict version-one WebSocket application messages and event envelopes.

use core::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CommandEnvelope, CommandId, CommandResult, ConnectionId, DesktopGeneration, DesktopId,
    ErrorCode, LeaseAcquireRequest, LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView,
    ProtocolVersion, RequestId, VersionRange,
};

/// Maximum UTF-8 byte length of one normalized event topic.
pub const MAX_EVENT_TOPIC_BYTES: usize = 128;
/// Maximum exact topics in one WebSocket subscription; an empty set means all.
pub const MAX_EVENT_TOPICS: usize = 32;
/// Maximum encoded normalized payload retained or delivered as one JSON event.
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;

/// Central command lifecycle topic emitted once per observed actor transition.
pub const COMMAND_LIFECYCLE_TOPIC: &str = "command.lifecycle";
/// Central backend action lifecycle topic.
pub const ACTION_LIFECYCLE_TOPIC: &str = "action.lifecycle";
/// Managed application exit after the owned leader was reaped.
pub const PROCESS_EXITED_TOPIC: &str = "process.exited";

/// A stable, bounded event topic assigned before subscriber filtering.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct EventTopic(
    #[schemars(
        length(min = 1, max = MAX_EVENT_TOPIC_BYTES),
        regex(pattern = "^[a-z0-9]+(?:[._-][a-z0-9]+)*$")
    )]
    String,
);

impl EventTopic {
    /// Creates a checked lowercase stable topic.
    pub fn new(value: impl Into<String>) -> Result<Self, WebSocketValidationError> {
        let topic = Self(value.into());
        topic.validate()?;
        Ok(topic)
    }

    /// Revalidates a topic obtained through deserialization.
    pub fn validate(&self) -> Result<(), WebSocketValidationError> {
        let bytes = self.0.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_EVENT_TOPIC_BYTES
            || !bytes.first().is_some_and(u8::is_ascii_lowercase)
            || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            || !bytes.iter().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || bytes.windows(2).any(|pair| {
                matches!(pair[0], b'.' | b'_' | b'-') && matches!(pair[1], b'.' | b'_' | b'-')
            })
        {
            return Err(WebSocketValidationError::EventTopic);
        }
        Ok(())
    }

    /// Returns the canonical topic string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventTopic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Client implementation identity reported during the bounded handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSocketClientDescriptor {
    /// Stable client/package name.
    pub name: String,
    /// Client release version.
    pub version: String,
}

/// Resume cursor bound to one exact desktop lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventResumeRequest {
    /// Desktop resource being resumed.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime that assigned `event_sequence`.
    pub desktop_generation: DesktopGeneration,
    /// Last globally assigned event sequence completely processed by the client.
    pub event_sequence: u64,
}

/// Required first WebSocket application message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    /// Must be exactly `client.hello`.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Correlates welcome and any replay status.
    pub request_id: RequestId,
    /// Client-supported protocol range.
    pub protocol: VersionRange,
    /// Bounded non-secret client identity.
    pub client: WebSocketClientDescriptor,
    /// Optional retained-event resume cursor.
    pub resume: Option<EventResumeRequest>,
}

impl ClientHello {
    /// Revalidates the strict handshake shape after deserialization.
    pub fn validate(&self) -> Result<(), WebSocketValidationError> {
        if self.message_type != "client.hello"
            || self.request_id.as_uuid().is_nil()
            || !valid_descriptor_part(&self.client.name)
            || !valid_descriptor_part(&self.client.version)
        {
            return Err(WebSocketValidationError::Hello);
        }
        if self.resume.as_ref().is_some_and(|resume| {
            resume.desktop_id.as_uuid().is_nil() || resume.desktop_generation.as_uuid().is_nil()
        }) {
            return Err(WebSocketValidationError::Hello);
        }
        Ok(())
    }
}

/// Strict post-handshake client messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", deny_unknown_fields)]
#[allow(missing_docs)]
pub enum WebSocketClientMessage {
    /// Application-level heartbeat.
    #[serde(rename = "client.ping")]
    Ping {
        request_id: RequestId,
        nonce: String,
    },
    /// Admit or idempotently retrieve a command.
    #[serde(rename = "command.submit")]
    CommandSubmit {
        request_id: RequestId,
        command: Box<CommandEnvelope>,
    },
    /// Begin delivery of a known command lifecycle.
    #[serde(rename = "command.watch")]
    CommandWatch {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        command_id: CommandId,
    },
    /// Stop delivery of a command lifecycle.
    #[serde(rename = "command.unwatch")]
    CommandUnwatch {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        command_id: CommandId,
    },
    /// Request cooperative command cancellation.
    #[serde(rename = "command.cancel")]
    CommandCancel {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        command_id: CommandId,
    },
    /// Read caller-redacted lease state.
    #[serde(rename = "lease.get")]
    LeaseGet {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    },
    /// Acquire the input lease.
    #[serde(rename = "lease.acquire")]
    LeaseAcquire {
        request_id: RequestId,
        lease: Box<LeaseAcquireRequest>,
    },
    /// Renew the input lease.
    #[serde(rename = "lease.renew")]
    LeaseRenew {
        request_id: RequestId,
        lease: Box<LeaseRenewRequest>,
    },
    /// Release the input lease and reset owned input.
    #[serde(rename = "lease.release")]
    LeaseRelease {
        request_id: RequestId,
        lease: Box<LeaseReleaseRequest>,
    },
    /// Replace the session's single bounded event subscription.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        /// Exact topics; empty means every topic authorized for the principal.
        topics: Vec<EventTopic>,
        /// Exclusive replay lower bound; `None` starts at the current live edge.
        since_sequence: Option<u64>,
    },
    /// Stop the session's event subscription.
    #[serde(rename = "events.unsubscribe")]
    EventsUnsubscribe {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    },
}

/// Coarse desktop state embedded in `server.welcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum WelcomeDesktopState {
    Booting,
    Probing,
    Ready,
    Degraded,
    Draining,
    Stopped,
    Failed,
}

/// Resume disposition advertised by `server.welcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum EventResumeStatus {
    Replayed,
    ResyncRequired,
    NotRequested,
}

/// Why a client must refresh authoritative snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum EventResyncReason {
    GenerationChanged,
    HistoryLost,
    SequenceAhead,
    SubscriberLag,
    OutboundBackpressure,
}

/// Authenticated identity summary in `server.welcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct WelcomePrincipal {
    pub id: String,
    pub capabilities: Vec<String>,
}

/// Current desktop summary in `server.welcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct WelcomeDesktop {
    pub id: DesktopId,
    pub generation: Option<DesktopGeneration>,
    pub state: WelcomeDesktopState,
}

/// Advertised connection-specific bounds in `server.welcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct WelcomeLimits {
    pub max_message_bytes: usize,
    pub heartbeat_ms: u64,
    pub normal_outbound_capacity: usize,
    pub reserved_outbound_capacity: usize,
    pub max_command_watches: usize,
}

/// Retained-resume status in `server.welcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct WelcomeResume {
    pub status: EventResumeStatus,
}

/// One normalized event after global sequence assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct NormalizedEvent {
    pub topic: EventTopic,
    /// Bounded, topic-specific, safe JSON data.
    pub payload: Value,
}

impl NormalizedEvent {
    /// Creates and validates normalized data before global sequence assignment.
    pub fn new(topic: EventTopic, payload: Value) -> Result<Self, WebSocketValidationError> {
        let event = Self { topic, payload };
        event.validate()?;
        Ok(event)
    }

    /// Revalidates topic and encoded payload bounds.
    pub fn validate(&self) -> Result<(), WebSocketValidationError> {
        self.topic.validate()?;
        let encoded = serde_json::to_vec(&self.payload)
            .map_err(|_| WebSocketValidationError::EventPayload)?;
        if encoded.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(WebSocketValidationError::EventPayload);
        }
        Ok(())
    }

    /// Returns a conservative retention charge including a fixed wire-envelope allowance.
    pub fn retention_charge(&self) -> Result<usize, WebSocketValidationError> {
        let encoded =
            serde_json::to_vec(self).map_err(|_| WebSocketValidationError::EventPayload)?;
        encoded
            .len()
            .checked_add(256)
            .ok_or(WebSocketValidationError::EventPayload)
    }
}

/// One normalized event after global sequence assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[allow(missing_docs)]
pub struct SequencedEvent {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    /// Global sequence assigned before subscriber filtering.
    pub sequence: u64,
    pub topic: EventTopic,
    /// Bounded, topic-specific, safe JSON data.
    pub payload: Value,
}

impl SequencedEvent {
    /// Revalidates identity, topic, and encoded payload bounds.
    pub fn validate(&self) -> Result<(), WebSocketValidationError> {
        if self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
            || self.sequence == 0
        {
            return Err(WebSocketValidationError::Event);
        }
        NormalizedEvent {
            topic: self.topic.clone(),
            payload: self.payload.clone(),
        }
        .validate()
    }
}

/// Server WebSocket application messages.
///
/// Unlike client inputs, decoders should tolerate additive fields from newer
/// minor protocol versions, so this output enum intentionally omits
/// `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
#[allow(missing_docs)]
pub enum WebSocketServerMessage {
    #[serde(rename = "server.welcome")]
    Welcome {
        protocol: ProtocolVersion,
        connection_id: ConnectionId,
        principal: WelcomePrincipal,
        desktop: WelcomeDesktop,
        limits: WelcomeLimits,
        resume: WelcomeResume,
    },
    #[serde(rename = "server.pong")]
    Pong {
        request_id: RequestId,
        nonce: String,
    },
    #[serde(rename = "command.accepted")]
    CommandAccepted {
        request_id: RequestId,
        result: CommandResult,
    },
    #[serde(rename = "command.progress")]
    CommandProgress {
        request_id: RequestId,
        result: CommandResult,
    },
    #[serde(rename = "command.result")]
    CommandResult {
        request_id: RequestId,
        result: CommandResult,
    },
    #[serde(rename = "command.unwatched")]
    CommandUnwatched {
        request_id: RequestId,
        command_id: CommandId,
        watching: bool,
    },
    #[serde(rename = "lease.state")]
    LeaseState {
        request_id: RequestId,
        lease: LeaseStateView,
    },
    #[serde(rename = "events.subscribed")]
    EventsSubscribed {
        request_id: RequestId,
        topics: Vec<EventTopic>,
    },
    #[serde(rename = "events.unsubscribed")]
    EventsUnsubscribed { request_id: RequestId },
    #[serde(rename = "event")]
    Event {
        request_id: RequestId,
        event: SequencedEvent,
    },
    #[serde(rename = "events.replay_complete")]
    EventsReplayComplete {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        through_sequence: u64,
    },
    #[serde(rename = "events.resync_required")]
    EventsResyncRequired {
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        reason: EventResyncReason,
        dropped_through: u64,
        latest_sequence: u64,
    },
    #[serde(rename = "server.draining")]
    ServerDraining {
        desktop_id: DesktopId,
        desktop_generation: Option<DesktopGeneration>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason_code: Option<String>,
    },
    #[serde(rename = "error")]
    Error {
        request_id: Option<RequestId>,
        code: ErrorCode,
        detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        desktop_generation: Option<DesktopGeneration>,
    },
}

impl WebSocketClientMessage {
    /// Revalidates bounds not fully expressible through Serde shape checks.
    pub fn validate(&self) -> Result<(), WebSocketValidationError> {
        match self {
            Self::Ping { request_id, nonce } => {
                if request_id.as_uuid().is_nil()
                    || nonce.is_empty()
                    || nonce.len() > 128
                    || nonce.chars().any(char::is_control)
                {
                    return Err(WebSocketValidationError::Message);
                }
            }
            Self::EventsSubscribe {
                request_id,
                desktop_id,
                desktop_generation,
                topics,
                ..
            } => {
                validate_target_ids(*request_id, *desktop_id, *desktop_generation)?;
                if topics.len() > MAX_EVENT_TOPICS {
                    return Err(WebSocketValidationError::EventTopics);
                }
                for (index, topic) in topics.iter().enumerate() {
                    topic.validate()?;
                    if topics[..index].contains(topic) {
                        return Err(WebSocketValidationError::EventTopics);
                    }
                }
            }
            Self::EventsUnsubscribe {
                request_id,
                desktop_id,
                desktop_generation,
            }
            | Self::LeaseGet {
                request_id,
                desktop_id,
                desktop_generation,
            } => validate_target_ids(*request_id, *desktop_id, *desktop_generation)?,
            Self::CommandWatch {
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            }
            | Self::CommandUnwatch {
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            }
            | Self::CommandCancel {
                request_id,
                desktop_id,
                desktop_generation,
                command_id,
            } => {
                validate_target_ids(*request_id, *desktop_id, *desktop_generation)?;
                if command_id.as_uuid().is_nil() {
                    return Err(WebSocketValidationError::Message);
                }
            }
            Self::CommandSubmit {
                request_id,
                command,
            } => {
                if request_id.as_uuid().is_nil()
                    || command.request_id != *request_id
                    || command.validate().is_err()
                {
                    return Err(WebSocketValidationError::Message);
                }
            }
            Self::LeaseAcquire { request_id, lease } => {
                if request_id.as_uuid().is_nil()
                    || lease.request_id != *request_id
                    || lease.validate().is_err()
                {
                    return Err(WebSocketValidationError::Message);
                }
            }
            Self::LeaseRenew { request_id, lease } => {
                if request_id.as_uuid().is_nil()
                    || lease.request_id != *request_id
                    || lease.validate().is_err()
                {
                    return Err(WebSocketValidationError::Message);
                }
            }
            Self::LeaseRelease { request_id, lease } => {
                if request_id.as_uuid().is_nil()
                    || lease.request_id != *request_id
                    || lease.validate().is_err()
                {
                    return Err(WebSocketValidationError::Message);
                }
            }
        }
        Ok(())
    }
}

fn validate_target_ids(
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), WebSocketValidationError> {
    if request_id.as_uuid().is_nil()
        || desktop_id.as_uuid().is_nil()
        || desktop_generation.as_uuid().is_nil()
    {
        return Err(WebSocketValidationError::Message);
    }
    Ok(())
}

fn valid_descriptor_part(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

/// Public WebSocket message validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[allow(missing_docs)]
pub enum WebSocketValidationError {
    #[error("client hello is invalid")]
    Hello,
    #[error("WebSocket client message is invalid")]
    Message,
    #[error("event topic is invalid")]
    EventTopic,
    #[error("event topic filter is invalid")]
    EventTopics,
    #[error("sequenced event is invalid")]
    Event,
    #[error("event payload exceeds its encoded bound")]
    EventPayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_event_subscription_rejects_unknown_fields_and_duplicate_topics()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let encoded = format!(
            r#"{{"type":"events.subscribe","request_id":"{request_id}","desktop_id":"{desktop_id}","desktop_generation":"{desktop_generation}","topics":["command.lifecycle"],"since_sequence":0}}"#
        );
        let message: WebSocketClientMessage = serde_json::from_str(&encoded)?;
        message.validate()?;
        assert!(
            serde_json::from_str::<WebSocketClientMessage>(
                &encoded.replace("}", ",\"typo\":true}")
            )
            .is_err()
        );

        let duplicated = encoded.replace(
            "[\"command.lifecycle\"]",
            "[\"command.lifecycle\",\"command.lifecycle\"]",
        );
        assert_eq!(
            serde_json::from_str::<WebSocketClientMessage>(&duplicated)?.validate(),
            Err(WebSocketValidationError::EventTopics)
        );
        Ok(())
    }

    #[test]
    fn resume_cursor_requires_generation_and_topics_are_bounded() {
        assert!(EventTopic::new(COMMAND_LIFECYCLE_TOPIC).is_ok());
        assert!(EventTopic::new(PROCESS_EXITED_TOPIC).is_ok());
        assert!(EventTopic::new("Command Lifecycle").is_err());
        assert!(EventTopic::new("command..lifecycle").is_err());
    }
}
