//! Secret-aware clipboard, selection-transfer, and text-insertion contracts.

use core::fmt;
use std::collections::BTreeSet;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::accessibility::{StrictElementRef, deserialize_strict_element_ref};
use crate::accessibility_action::{
    EditableTextSelectionPolicy, ElementPostcondition, MAX_EDITABLE_TEXT_OFFSET,
};
use crate::artifact::{StrictArtifactRef, deserialize_strict_artifact_ref};
use crate::window::{
    StrictWindowRef, deserialize_optional_strict_window_ref, deserialize_strict_window_ref,
};
use crate::{
    AccessibilityRevision, ArtifactPurpose, ArtifactRef, DesktopGeneration, DesktopId, ElementRef,
    Sha256Digest, WindowRef,
};

fn deserialize_boxed_strict_element_ref<'de, D>(
    deserializer: D,
) -> Result<Box<ElementRef>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_strict_element_ref(deserializer).map(Box::new)
}

/// Largest inline clipboard body carried in an ordinary JSON message.
pub const MAX_INLINE_CLIPBOARD_BYTES: usize = 256 * 1_024;
/// Largest clipboard selection body accepted by the X11 actor.
pub const MAX_SELECTION_BYTES: u64 = 16 * 1_024 * 1_024;
/// Maximum previous text value copied during paste preservation.
pub const MAX_CLIPBOARD_PRESERVATION_BYTES: u64 = 4 * 1_024 * 1_024;
/// Default maximum text insertion body.
pub const MAX_TEXT_INSERT_BYTES: u64 = 1_024 * 1_024;
/// Maximum target atom name exposed through the API.
pub const MAX_CLIPBOARD_TARGET_BYTES: usize = 128;
/// Maximum preferred/requested targets retained in a result.
pub const MAX_CLIPBOARD_TARGETS: usize = 32;
/// Maximum observed INCR chunk count retained as evidence.
pub const MAX_INCR_CHUNKS: u32 = 4_096;
/// Maximum paste-observation window.
pub const MAX_PASTE_OBSERVATION_TIMEOUT_MS: u32 = 2_000;
/// Closed target vocabulary accepted from public requests. Internal protocol
/// atoms such as TARGETS, TIMESTAMP, MULTIPLE, and INCR are never interned from
/// caller text.
pub const SUPPORTED_CLIPBOARD_TARGETS: [&str; 6] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
    "application/octet-stream",
    "image/png",
];

/// Independently owned X11 selections supported for release one writes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SelectionName {
    /// Explicit clipboard selection.
    Clipboard,
    /// Pointer-selection convention; never mirrored automatically.
    Primary,
}

/// A bounded X11 target atom name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "clipboard_target_schema")]
pub struct ClipboardTarget(String);

fn clipboard_target_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_CLIPBOARD_TARGET_BYTES,
        "pattern": "^[!-~]+$"
    })
}

impl ClipboardTarget {
    /// Creates a bounded printable target name.
    pub fn new(value: impl Into<String>) -> Result<Self, ClipboardValidationError> {
        let value = value.into();
        if value.len() > MAX_CLIPBOARD_TARGET_BYTES
            || !SUPPORTED_CLIPBOARD_TARGETS.contains(&value.as_str())
        {
            return Err(ClipboardValidationError::Target);
        }
        Ok(Self(value))
    }

    /// Returns the exact target name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClipboardTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ClipboardTarget")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ClipboardTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ClipboardTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Inline UTF-8 content whose diagnostics expose only byte/scalar counts.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
#[schemars(schema_with = "secret_inline_text_schema")]
pub struct SecretInlineText(String);

fn secret_inline_text_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "maxLength": MAX_INLINE_CLIPBOARD_BYTES
    })
}

impl SecretInlineText {
    /// Creates bounded inline UTF-8 text. Empty selection values are valid.
    pub fn new(value: impl Into<String>) -> Result<Self, ClipboardValidationError> {
        let value = value.into();
        if value.len() > MAX_INLINE_CLIPBOARD_BYTES {
            return Err(ClipboardValidationError::InlinePayload);
        }
        Ok(Self(value))
    }

    /// Returns content only at an explicitly authorized effect boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Exact UTF-8 byte length.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.0.len()
    }

    /// Unicode scalar-value count, deliberately not a grapheme claim.
    #[must_use]
    pub fn scalar_count(&self) -> usize {
        self.0.chars().count()
    }
}

impl fmt::Debug for SecretInlineText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretInlineText")
            .field("utf8_bytes", &self.byte_len())
            .field("unicode_scalars", &self.scalar_count())
            .finish_non_exhaustive()
    }
}

impl Serialize for SecretInlineText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretInlineText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Base64-encoded inline binary data with independently checked length/hash.
#[derive(Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SecretInlineBinary {
    base64: String,
    #[schemars(range(max = MAX_INLINE_CLIPBOARD_BYTES))]
    decoded_length: u32,
    sha256: Sha256Digest,
}

impl SecretInlineBinary {
    /// Creates checked standard-alphabet padded base64 metadata.
    pub fn new(
        base64: impl Into<String>,
        decoded_length: u32,
        sha256: Sha256Digest,
    ) -> Result<Self, ClipboardValidationError> {
        let value = Self {
            base64: base64.into(),
            decoded_length,
            sha256,
        };
        value.validate()?;
        Ok(value)
    }

    /// Revalidates alphabet, padding, decoded length, and inline ceiling.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        let decoded = decoded_base64_length(&self.base64)?;
        if decoded > MAX_INLINE_CLIPBOARD_BYTES || decoded != self.decoded_length as usize {
            return Err(ClipboardValidationError::InlinePayload);
        }
        let actual = base64_sha256(&self.base64)?;
        if actual.as_str() != self.sha256.as_str() {
            return Err(ClipboardValidationError::InlinePayload);
        }
        Ok(())
    }

    /// Returns base64 only at an explicitly authorized effect boundary.
    #[must_use]
    pub fn expose_base64_secret(&self) -> &str {
        &self.base64
    }

    /// Exact decoded byte length.
    #[must_use]
    pub const fn decoded_length(&self) -> u32 {
        self.decoded_length
    }

    /// Expected decoded-content identity.
    #[must_use]
    pub const fn sha256(&self) -> &Sha256Digest {
        &self.sha256
    }
}

impl fmt::Debug for SecretInlineBinary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretInlineBinary")
            .field("decoded_length", &self.decoded_length)
            .field("base64", &"[REDACTED]")
            .field("sha256", &"[REDACTED]")
            .finish()
    }
}

impl<'de> Deserialize<'de> for SecretInlineBinary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            base64: String,
            decoded_length: u32,
            sha256: Sha256Digest,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.base64, wire.decoded_length, wire.sha256).map_err(de::Error::custom)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "SecretInlineBinary")]
struct StrictSecretInlineBinary {
    base64: String,
    #[schemars(range(max = MAX_INLINE_CLIPBOARD_BYTES))]
    decoded_length: u32,
    sha256: Sha256Digest,
}

fn deserialize_strict_secret_inline_binary<'de, D>(
    deserializer: D,
) -> Result<SecretInlineBinary, D::Error>
where
    D: Deserializer<'de>,
{
    let value = StrictSecretInlineBinary::deserialize(deserializer)?;
    SecretInlineBinary::new(value.base64, value.decoded_length, value.sha256)
        .map_err(de::Error::custom)
}

/// Content accepted by selection writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClipboardWriteSource {
    /// Inline UTF-8 represented through the standard text targets.
    InlineText {
        /// Secret text bytes.
        text: SecretInlineText,
    },
    /// Inline binary bytes for one explicitly allowed MIME target.
    InlineBinary {
        /// Target atom/media type.
        target: ClipboardTarget,
        /// Secret base64 body with checked decoded length/hash.
        #[serde(deserialize_with = "deserialize_strict_secret_inline_binary")]
        #[schemars(with = "StrictSecretInlineBinary")]
        data: SecretInlineBinary,
    },
    /// Private immutable upload, revalidated at command execution.
    Artifact {
        /// Must have purpose `clipboard_input` and fit the selection ceiling.
        #[serde(deserialize_with = "deserialize_strict_artifact_ref")]
        #[schemars(with = "StrictArtifactRef")]
        artifact: ArtifactRef,
        /// Target atom/media type to serve without decoding.
        target: ClipboardTarget,
    },
}

impl ClipboardWriteSource {
    /// Revalidates inline bounds or artifact purpose and length.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        match self {
            Self::InlineText { text } => SecretInlineText::new(text.expose_secret()).map(|_| ()),
            Self::InlineBinary { target, data } => {
                ClipboardTarget::new(target.as_str())?;
                data.validate()
            }
            Self::Artifact { artifact, target } => {
                ClipboardTarget::new(target.as_str())?;
                artifact
                    .validate()
                    .map_err(|_| ClipboardValidationError::Artifact)?;
                if artifact.purpose != ArtifactPurpose::ClipboardInput
                    || artifact.content_length > MAX_SELECTION_BYTES
                {
                    return Err(ClipboardValidationError::Artifact);
                }
                Ok(())
            }
        }
    }

    fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), ClipboardValidationError> {
        self.validate()?;
        if let Self::Artifact { artifact, .. } = self {
            validate_artifact_scope(artifact, desktop_id, desktop_generation)?;
        }
        Ok(())
    }
}

/// Acquire and continuously serve one selection value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionSetCommand {
    /// CLIPBOARD and PRIMARY remain independent.
    pub selection: SelectionName,
    /// Secret inline data or private input artifact.
    pub content: ClipboardWriteSource,
}

impl SelectionSetCommand {
    /// Validates the content source.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        self.content.validate()
    }

    /// Performs shape validation and binds an artifact source to the route's
    /// desktop lifetime. Live ownership, expiry, and digest checks remain the
    /// artifact store's responsibility at effect time.
    pub fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), ClipboardValidationError> {
        self.content
            .validate_for_desktop(desktop_id, desktop_generation)
    }
}

/// Relinquish Xenoteer's ownership of one selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionClearCommand {
    /// Selection to clear without affecting the other selection.
    pub selection: SelectionName,
}

/// Read another owner's preferred bounded representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClipboardReadRequest {
    /// Selection to convert.
    pub selection: SelectionName,
    /// Ordered target preferences; empty selects the server's documented text order.
    #[schemars(length(max = MAX_CLIPBOARD_TARGETS))]
    pub preferred_targets: Vec<ClipboardTarget>,
    /// Whether invalid text encoding may be returned as binary rather than rejected.
    pub allow_binary_fallback: bool,
}

impl ClipboardReadRequest {
    /// Requires a bounded, unique preference list.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        validate_targets(&self.preferred_targets)
    }
}

/// Direct versus ICCCM INCR transfer evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SelectionTransferMode {
    /// One ordinary property transfer.
    Direct,
    /// Property-delete handshake with one or more chunks and a zero-length terminator.
    Incr {
        /// Untrusted lower-bound byte count announced by the peer.
        #[schemars(range(max = MAX_SELECTION_BYTES))]
        announced_minimum_bytes: u64,
        /// Non-terminal content chunks processed.
        #[schemars(range(max = MAX_INCR_CHUNKS))]
        chunks: u32,
    },
}

/// Terminal status of a direct or INCR selection transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SelectionTransferTerminal {
    /// The exact payload and, for INCR, its zero-length terminator were observed.
    Completed,
    /// A compatible request was observed but the transfer ended without success.
    Failed {
        /// Bounded content-free failure category.
        reason: SelectionTransferFailureReason,
    },
}

/// Why an observed selection transfer terminated before completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelectionTransferFailureReason {
    /// The bounded peer handshake deadline elapsed.
    Timeout,
    /// Selection ownership changed during the transfer.
    OwnerChanged,
    /// The requestor window disappeared.
    RequestorDestroyed,
    /// The peer violated direct, MULTIPLE, or INCR framing semantics.
    ProtocolViolation,
    /// Accumulated bytes crossed the configured selection ceiling.
    SelectionTooLarge,
    /// The enclosing command was cancelled before completion.
    Cancelled,
}

/// Content-free evidence that an X11 selection transfer reached a terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelectionTransferEvidence {
    /// Representation actually transferred.
    pub target: ClipboardTarget,
    /// Direct or INCR wire path.
    pub transfer: SelectionTransferMode,
    /// Exact accumulated payload bytes, never trusted from INCR advertisement.
    #[schemars(range(max = MAX_SELECTION_BYTES))]
    pub content_length: u64,
    /// Content identity; content itself is absent from diagnostics/events.
    pub sha256: Sha256Digest,
    /// Whether the selection owner changed during the transfer.
    pub owner_changed: bool,
    /// Whether the required zero-length terminator was observed for INCR.
    pub terminal_chunk_observed: bool,
    /// Completed or content-free failed terminal outcome.
    pub terminal: SelectionTransferTerminal,
}

impl SelectionTransferEvidence {
    /// Revalidates lengths and direct/INCR completion semantics.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        ClipboardTarget::new(self.target.as_str())?;
        Sha256Digest::new(self.sha256.as_str())
            .map_err(|_| ClipboardValidationError::TransferEvidence)?;
        if self.content_length > MAX_SELECTION_BYTES {
            return Err(ClipboardValidationError::SelectionTooLarge);
        }
        let mode_is_consistent = match (self.transfer, self.terminal) {
            (SelectionTransferMode::Direct, SelectionTransferTerminal::Completed) => {
                !self.terminal_chunk_observed
            }
            (
                SelectionTransferMode::Incr {
                    announced_minimum_bytes,
                    chunks,
                },
                SelectionTransferTerminal::Completed,
            ) => {
                chunks > 0
                    && chunks <= MAX_INCR_CHUNKS
                    && self.terminal_chunk_observed
                    && announced_minimum_bytes <= self.content_length
            }
            (SelectionTransferMode::Direct, SelectionTransferTerminal::Failed { .. }) => {
                !self.terminal_chunk_observed
            }
            (
                SelectionTransferMode::Incr { chunks, .. },
                SelectionTransferTerminal::Failed { .. },
            ) => chunks <= MAX_INCR_CHUNKS && !self.terminal_chunk_observed,
        };
        let ownership_is_consistent = match self.terminal {
            SelectionTransferTerminal::Completed => !self.owner_changed,
            SelectionTransferTerminal::Failed {
                reason: SelectionTransferFailureReason::OwnerChanged,
            } => self.owner_changed,
            SelectionTransferTerminal::Failed { .. } => true,
        };
        if !mode_is_consistent || !ownership_is_consistent {
            return Err(ClipboardValidationError::TransferEvidence);
        }
        Ok(())
    }

    /// Returns whether the transfer reached a successful terminal state.
    #[must_use]
    pub const fn completed(&self) -> bool {
        matches!(self.terminal, SelectionTransferTerminal::Completed)
    }
}

/// Authorized clipboard-read output, inline only below the ordinary message bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum ClipboardReadDelivery {
    /// Valid UTF-8 body carried inline.
    InlineText {
        /// Secret text bytes.
        text: SecretInlineText,
    },
    /// Binary response carried as checked base64.
    InlineBinary {
        /// Secret binary body.
        data: SecretInlineBinary,
    },
    /// Larger private output fetched through the artifact API.
    Artifact {
        /// Must have purpose `clipboard_output`.
        artifact: ArtifactRef,
    },
}

impl ClipboardReadDelivery {
    /// Revalidates content bounds and artifact purpose.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        match self {
            Self::InlineText { text } => SecretInlineText::new(text.expose_secret()).map(|_| ()),
            Self::InlineBinary { data } => data.validate(),
            Self::Artifact { artifact } => {
                artifact
                    .validate()
                    .map_err(|_| ClipboardValidationError::Artifact)?;
                if artifact.purpose != ArtifactPurpose::ClipboardOutput {
                    return Err(ClipboardValidationError::Artifact);
                }
                Ok(())
            }
        }
    }
}

/// One completed clipboard read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClipboardReadResult {
    /// Selection converted.
    pub selection: SelectionName,
    /// Actor-local monotonic selection revision.
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    pub revision: u64,
    /// Wire transfer evidence.
    pub evidence: SelectionTransferEvidence,
    /// Inline authorized body or private output artifact.
    pub content: ClipboardReadDelivery,
}

impl ClipboardReadResult {
    /// Revalidates revision, transfer metadata, and delivery length.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        if self.revision == 0 {
            return Err(ClipboardValidationError::Revision);
        }
        self.evidence.validate()?;
        if !self.evidence.completed() {
            return Err(ClipboardValidationError::TransferEvidence);
        }
        self.content.validate()?;
        let delivered_length = match &self.content {
            ClipboardReadDelivery::InlineText { text } => {
                let actual = sha256_bytes(text.expose_secret().as_bytes())?;
                if actual.as_str() != self.evidence.sha256.as_str() {
                    return Err(ClipboardValidationError::TransferEvidence);
                }
                text.byte_len() as u64
            }
            ClipboardReadDelivery::InlineBinary { data } => {
                if data.sha256() != &self.evidence.sha256 {
                    return Err(ClipboardValidationError::TransferEvidence);
                }
                u64::from(data.decoded_length())
            }
            ClipboardReadDelivery::Artifact { artifact } => {
                if artifact.sha256.as_str() != self.evidence.sha256.as_str() {
                    return Err(ClipboardValidationError::TransferEvidence);
                }
                artifact.content_length
            }
        };
        if delivered_length != self.evidence.content_length {
            return Err(ClipboardValidationError::TransferEvidence);
        }
        Ok(())
    }

    /// Performs shape validation plus desktop/generation comparison for a route.
    ///
    /// This does not prove artifact ownership, current expiry, stored bytes, or
    /// authorization; the artifact store must revalidate those live properties.
    pub fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), ClipboardValidationError> {
        self.validate()?;
        if let ClipboardReadDelivery::Artifact { artifact } = &self.content {
            validate_artifact_scope(artifact, desktop_id, desktop_generation)?;
        }
        Ok(())
    }
}

/// Clipboard state left after temporary paste cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardRestorationKind {
    /// Caller explicitly disabled preservation.
    NotRequested,
    /// Xenoteer remains owner of a copied previous text value.
    ValueCopy,
    /// No owner/value existed, so Xenoteer relinquished ownership.
    RelinquishedNoOwner,
    /// A bounded text value was restored but arbitrary formats were not.
    PartialValueCopy,
    /// Restoration could not be proven.
    Failed,
}

/// Content-free clipboard restoration evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClipboardRestorationEvidence {
    /// Whether preservation was requested.
    pub requested: bool,
    /// Whether an owner existed before Xenoteer acquired CLIPBOARD.
    pub previous_owner_existed: bool,
    /// Bytes copied within the preservation budget.
    #[schemars(range(max = MAX_CLIPBOARD_PRESERVATION_BYTES))]
    pub preserved_bytes: u64,
    /// Honest restoration semantics; never claims original-owner restoration.
    pub kind: ClipboardRestorationKind,
}

impl ClipboardRestorationEvidence {
    /// Rejects contradictory preservation claims.
    pub fn validate(self) -> Result<(), ClipboardValidationError> {
        let valid = match self.kind {
            ClipboardRestorationKind::NotRequested => !self.requested && self.preserved_bytes == 0,
            ClipboardRestorationKind::RelinquishedNoOwner => {
                self.requested && !self.previous_owner_existed && self.preserved_bytes == 0
            }
            ClipboardRestorationKind::ValueCopy | ClipboardRestorationKind::PartialValueCopy => {
                self.requested && self.previous_owner_existed
            }
            ClipboardRestorationKind::Failed => self.requested,
        };
        if !valid || self.preserved_bytes > MAX_CLIPBOARD_PRESERVATION_BYTES {
            return Err(ClipboardValidationError::RestorationEvidence);
        }
        Ok(())
    }
}

/// Content-free evidence that an application requested temporary paste data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClipboardPasteEvidence {
    /// At least one compatible SelectionRequest was observed.
    pub request_observed: bool,
    /// Ordered unique target names requested by the application.
    #[schemars(length(max = MAX_CLIPBOARD_TARGETS))]
    pub requested_targets: Vec<ClipboardTarget>,
    /// Terminal transfer evidence when a compatible request was served. A
    /// failed terminal preserves partial INCR progress without claiming success.
    pub transfer: Option<SelectionTransferEvidence>,
    /// Optional independently evaluated semantic/visual postcondition.
    pub postcondition_met: Option<bool>,
    /// Clipboard cleanup result.
    pub restoration: ClipboardRestorationEvidence,
}

impl ClipboardPasteEvidence {
    /// Requires terminal transfer evidence exactly when a compatible request was observed.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        validate_targets(&self.requested_targets)?;
        if self.request_observed != self.transfer.is_some()
            || (!self.request_observed && !self.requested_targets.is_empty())
        {
            return Err(ClipboardValidationError::PasteEvidence);
        }
        if let Some(transfer) = &self.transfer {
            transfer.validate()?;
            if !self.requested_targets.contains(&transfer.target) {
                return Err(ClipboardValidationError::PasteEvidence);
            }
        }
        self.restoration.validate()?;
        Ok(())
    }
}

/// Concrete text strategies plus the bounded policy-selection sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextStrategy {
    /// AT-SPI EditableText insertion against an exact element generation.
    Semantic,
    /// Current-layout XTEST keycodes only.
    Physical,
    /// Temporary CLIPBOARD ownership plus an observed paste request.
    Clipboard,
    /// Explicit global temporary-keymap strategy.
    PhysicalExtended,
    /// Deterministic selection from an explicit or legacy ordered policy.
    Auto,
}

impl TextStrategy {
    const fn policy_rank(self) -> Option<u8> {
        match self {
            Self::Semantic => Some(0),
            Self::Physical => Some(1),
            Self::Clipboard => Some(2),
            Self::PhysicalExtended => Some(3),
            Self::Auto => None,
        }
    }

    /// Whether this concrete strategy uses the exclusive physical-input plane.
    #[must_use]
    pub const fn requires_control_lease(self) -> bool {
        matches!(
            self,
            Self::Physical | Self::Clipboard | Self::PhysicalExtended | Self::Auto
        )
    }
}

/// Text body carried inline or by a purpose-bound private upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TextSource {
    /// Inline UTF-8 under the ordinary-message ceiling.
    Inline {
        /// Secret text.
        text: SecretInlineText,
    },
    /// Larger immutable UTF-8 upload.
    Artifact {
        /// Must be a `clipboard_input` artifact of at most 1 MiB.
        #[serde(deserialize_with = "deserialize_strict_artifact_ref")]
        #[schemars(with = "StrictArtifactRef")]
        artifact: ArtifactRef,
    },
}

impl TextSource {
    /// Returns the exact UTF-8 byte count promised by the source.
    pub fn validate(&self) -> Result<u64, ClipboardValidationError> {
        match self {
            Self::Inline { text } => {
                SecretInlineText::new(text.expose_secret())?;
                Ok(text.byte_len() as u64)
            }
            Self::Artifact { artifact } => {
                artifact
                    .validate()
                    .map_err(|_| ClipboardValidationError::Artifact)?;
                if artifact.purpose != ArtifactPurpose::ClipboardInput
                    || artifact.content_length > MAX_TEXT_INSERT_BYTES
                    || artifact.content_type.as_str() != "text/plain;charset=utf-8"
                {
                    return Err(ClipboardValidationError::Artifact);
                }
                Ok(artifact.content_length)
            }
        }
    }

    fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<u64, ClipboardValidationError> {
        let bytes = self.validate()?;
        if let Self::Artifact { artifact } = self {
            validate_artifact_scope(artifact, desktop_id, desktop_generation)?;
        }
        Ok(bytes)
    }
}

/// Target whose focus/identity is revalidated before text effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum TextTarget {
    /// Exact generation-bound X11 window.
    Window {
        /// Window to activate/verify before insertion.
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    /// Exact AT-SPI element, with an explicit correlated X11 fallback when a
    /// policy may synthesize physical input or request clipboard paste.
    Element {
        /// Generation-fenced semantic target.
        #[serde(deserialize_with = "deserialize_boxed_strict_element_ref")]
        #[schemars(with = "StrictElementRef")]
        element: Box<ElementRef>,
        /// Exact X11 target for any permitted non-semantic fallback.
        #[serde(default, deserialize_with = "deserialize_optional_strict_window_ref")]
        #[schemars(with = "Option<StrictWindowRef>")]
        window_fallback: Option<WindowRef>,
    },
}

/// Where AT-SPI EditableText inserts the first scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticTextInsertionPoint {
    /// Resolve the live caret immediately before the semantic effect.
    Caret,
    /// Insert at this non-negative AT-SPI character offset.
    Offset {
        /// AT-SPI character offset, not a UTF-8 byte offset.
        #[schemars(range(min = 0, max = MAX_EDITABLE_TEXT_OFFSET))]
        offset: i32,
    },
}

impl SemanticTextInsertionPoint {
    fn validate(self) -> Result<(), ClipboardValidationError> {
        if let Self::Offset { offset } = self
            && !(0..=MAX_EDITABLE_TEXT_OFFSET).contains(&offset)
        {
            return Err(ClipboardValidationError::SemanticOptions);
        }
        Ok(())
    }
}

/// Bounded AT-SPI EditableText behavior. There is intentionally no
/// `allow_protected` or equivalent caller-controlled policy bypass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticTextInsertOptions {
    /// Live caret or explicit character offset.
    pub insertion_point: SemanticTextInsertionPoint,
    /// Selection handling around the insertion.
    pub selection: EditableTextSelectionPolicy,
    /// Verify only character counts/caret/selection metadata, never content.
    pub verify_length_only: bool,
    /// Optional content-free semantic postcondition.
    pub postcondition: Option<ElementPostcondition>,
}

impl SemanticTextInsertOptions {
    /// Validates the insertion point and bounded postcondition.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        self.insertion_point.validate()?;
        if let Some(postcondition) = &self.postcondition {
            postcondition
                .validate()
                .map_err(|_| ClipboardValidationError::SemanticOptions)?;
        }
        Ok(())
    }
}

/// Auto may advance only when the preceding strategy produced no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextAutoFallbackPolicy {
    /// Stop selection permanently once any concrete strategy has an effect.
    BeforeEffectOnly,
}

/// Explicit bounded strategy union in canonical evaluation order:
/// semantic, current-layout physical, clipboard, physical-extended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextAutoPolicy {
    /// Non-empty canonical subset of concrete strategies.
    #[schemars(length(min = 1, max = 4))]
    pub allowed_strategies: Vec<TextStrategy>,
    /// Explicit no-fallback-after-effect rule.
    pub fallback: TextAutoFallbackPolicy,
}

impl TextAutoPolicy {
    /// Requires a non-empty, duplicate-free canonical concrete-strategy list.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        if self.allowed_strategies.is_empty() || self.allowed_strategies.len() > 4 {
            return Err(ClipboardValidationError::AutoPolicy);
        }
        let mut previous = None;
        for strategy in &self.allowed_strategies {
            let rank = strategy
                .policy_rank()
                .ok_or(ClipboardValidationError::AutoPolicy)?;
            if previous.is_some_and(|previous| rank <= previous) {
                return Err(ClipboardValidationError::AutoPolicy);
            }
            previous = Some(rank);
        }
        Ok(())
    }

    /// Whether the policy may select this concrete strategy.
    #[must_use]
    pub fn permits(&self, strategy: TextStrategy) -> bool {
        strategy != TextStrategy::Auto && self.allowed_strategies.contains(&strategy)
    }
}

/// Union of concrete text planes used by admission/authentication decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStrategyUnion {
    /// Semantic accessibility write may be selected.
    pub semantic: bool,
    /// Current-layout physical typing may be selected.
    pub physical: bool,
    /// Clipboard paste may be selected.
    pub clipboard: bool,
    /// Temporary-keymap physical typing may be selected.
    pub physical_extended: bool,
}

impl TextStrategyUnion {
    const EMPTY: Self = Self {
        semantic: false,
        physical: false,
        clipboard: false,
        physical_extended: false,
    };

    fn insert(&mut self, strategy: TextStrategy) {
        match strategy {
            TextStrategy::Semantic => self.semantic = true,
            TextStrategy::Physical => self.physical = true,
            TextStrategy::Clipboard => self.clipboard = true,
            TextStrategy::PhysicalExtended => self.physical_extended = true,
            TextStrategy::Auto => {}
        }
    }

    /// Whether any permitted strategy needs the controller lease.
    #[must_use]
    pub const fn requires_control_lease(self) -> bool {
        self.physical || self.clipboard || self.physical_extended
    }

    /// Whether semantic accessibility write authority may be exercised.
    #[must_use]
    pub const fn requires_accessibility_write(self) -> bool {
        self.semantic
    }
}

/// Clipboard-specific text insertion policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextInsertOptions {
    /// Preserve a bounded text representation of the previous CLIPBOARD value.
    pub preserve_clipboard: bool,
    /// Maximum wait for a compatible application SelectionRequest.
    #[schemars(range(min = 1, max = MAX_PASTE_OBSERVATION_TIMEOUT_MS))]
    pub paste_observation_timeout_ms: u32,
}

impl TextInsertOptions {
    /// Validates the bounded paste observation interval.
    pub fn validate(self) -> Result<(), ClipboardValidationError> {
        if self.paste_observation_timeout_ms == 0
            || self.paste_observation_timeout_ms > MAX_PASTE_OBSERVATION_TIMEOUT_MS
        {
            return Err(ClipboardValidationError::PasteTimeout);
        }
        Ok(())
    }
}

/// Insert exact UTF-8 using one declared strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextInsertCommand {
    /// Secret inline text or private artifact.
    pub text: TextSource,
    /// Exact target revalidated near execution.
    pub target: TextTarget,
    /// Requested strategy, including deterministic `auto`.
    pub strategy: TextStrategy,
    /// Clipboard-specific bounds, present only when clipboard might be selected.
    pub clipboard_options: Option<TextInsertOptions>,
    /// Semantic options, present exactly when semantic insertion may be selected.
    pub semantic_options: Option<SemanticTextInsertOptions>,
    /// Explicit policy required for element-target `auto`. Omission on a
    /// window-target `auto` preserves the legacy physical/clipboard/extended
    /// policy and its existing wire shape.
    pub auto_policy: Option<TextAutoPolicy>,
}

impl TextInsertCommand {
    /// Revalidates source size and bounded strategy options.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        let bytes = self.text.validate()?;
        if bytes > MAX_TEXT_INSERT_BYTES {
            return Err(ClipboardValidationError::TextTooLarge);
        }
        self.target.validate()?;
        self.validate_strategy_options()
    }

    /// Performs structural validation and binds artifact/window references to
    /// the route's desktop lifetime.
    ///
    /// The observation actor must still prove the exact window birth is live,
    /// and the artifact store must prove principal ownership, expiry, and hash.
    pub fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), ClipboardValidationError> {
        let bytes = self
            .text
            .validate_for_desktop(desktop_id, desktop_generation)?;
        if bytes > MAX_TEXT_INSERT_BYTES {
            return Err(ClipboardValidationError::TextTooLarge);
        }
        self.target
            .validate_for_desktop(desktop_id, desktop_generation)?;
        self.validate_strategy_options()
    }

    /// Returns the complete union of concrete strategies that authorization
    /// must allow before dispatch. Invalid requests still require validation.
    #[must_use]
    pub fn strategy_union(&self) -> TextStrategyUnion {
        let mut union = TextStrategyUnion::EMPTY;
        match self.strategy {
            TextStrategy::Auto => {
                if let Some(policy) = &self.auto_policy {
                    for strategy in &policy.allowed_strategies {
                        union.insert(*strategy);
                    }
                } else if matches!(self.target, TextTarget::Window { .. }) {
                    // Backwards-compatible release-four window Auto policy.
                    union.insert(TextStrategy::Physical);
                    union.insert(TextStrategy::Clipboard);
                    union.insert(TextStrategy::PhysicalExtended);
                }
            }
            strategy => union.insert(strategy),
        }
        union
    }

    /// Whether admission requires the exclusive physical controller lease.
    #[must_use]
    pub fn requires_control_lease(&self) -> bool {
        self.strategy_union().requires_control_lease()
    }

    fn validate_strategy_options(&self) -> Result<(), ClipboardValidationError> {
        let union = self.strategy_union();
        if self.strategy == TextStrategy::Auto {
            match (&self.target, &self.auto_policy) {
                (TextTarget::Window { .. }, None) => {}
                (TextTarget::Window { .. } | TextTarget::Element { .. }, Some(policy)) => {
                    policy.validate()?;
                }
                (TextTarget::Element { .. }, None) => {
                    return Err(ClipboardValidationError::AutoPolicy);
                }
            }
        } else if self.auto_policy.is_some() {
            return Err(ClipboardValidationError::AutoPolicy);
        }

        if union.clipboard != self.clipboard_options.is_some()
            || union.semantic != self.semantic_options.is_some()
        {
            return Err(ClipboardValidationError::TextOptions);
        }
        if let Some(options) = self.clipboard_options {
            options.validate()?;
        }
        if let Some(options) = &self.semantic_options {
            options.validate()?;
        }

        match &self.target {
            TextTarget::Window { .. } if union.semantic => {
                Err(ClipboardValidationError::TextTarget)
            }
            TextTarget::Element {
                window_fallback, ..
            } if union.requires_control_lease() != window_fallback.is_some() => {
                Err(ClipboardValidationError::TextTarget)
            }
            TextTarget::Window { .. } | TextTarget::Element { .. } => Ok(()),
        }
    }
}

impl TextTarget {
    /// Revalidates the generation-bound target reference.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        match self {
            Self::Window { window } => window
                .validate()
                .map_err(|_| ClipboardValidationError::TextTarget),
            Self::Element {
                element,
                window_fallback,
            } => {
                element
                    .validate()
                    .map_err(|_| ClipboardValidationError::TextTarget)?;
                if let Some(window) = window_fallback {
                    window
                        .validate()
                        .map_err(|_| ClipboardValidationError::TextTarget)?;
                    if window.desktop_id != element.desktop_id
                        || window.desktop_generation != element.desktop_generation
                    {
                        return Err(ClipboardValidationError::ReferenceScope);
                    }
                }
                Ok(())
            }
        }
    }

    fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), ClipboardValidationError> {
        self.validate()?;
        match self {
            Self::Window { window }
                if window.desktop_id == desktop_id
                    && window.desktop_generation == desktop_generation =>
            {
                Ok(())
            }
            Self::Window { .. } => Err(ClipboardValidationError::ReferenceScope),
            Self::Element {
                element,
                window_fallback,
            } if element.desktop_id == desktop_id
                && element.desktop_generation == desktop_generation
                && window_fallback.as_ref().is_none_or(|window| {
                    window.desktop_id == desktop_id
                        && window.desktop_generation == desktop_generation
                }) =>
            {
                Ok(())
            }
            Self::Element { .. } => Err(ClipboardValidationError::ReferenceScope),
        }
    }
}

/// Content-free AT-SPI EditableText acceptance and readback evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SemanticTextInsertEvidence {
    /// Exact element generation accepted by the backend.
    pub element: ElementRef,
    /// Actor-owned revision immediately before insertion.
    pub revision_before: AccessibilityRevision,
    /// Actor-owned revision after readback.
    pub revision_after: AccessibilityRevision,
    /// Whether EditableText accepted the insertion call.
    pub backend_accepted: bool,
    /// Resolved character offset used for the effect.
    pub insertion_offset: u32,
    /// Content-free character-count readback before insertion.
    pub character_count_before: u32,
    /// Content-free character-count readback after insertion.
    pub character_count_after: u32,
    /// Optional caret metadata read back after insertion.
    pub caret_offset_after: Option<u32>,
    /// Optional selection-count metadata read back after insertion.
    pub selection_count_after: Option<u32>,
    /// Whether verification intentionally inspected lengths only.
    pub verified_length_only: bool,
    /// Independently evaluated postcondition result, when requested.
    pub postcondition_satisfied: Option<bool>,
}

impl SemanticTextInsertEvidence {
    /// Validates monotonic revisions and content-free length/caret readback.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        self.element
            .validate()
            .map_err(|_| ClipboardValidationError::TextEvidence)?;
        if self.revision_after < self.revision_before
            || !self.backend_accepted
            || self.insertion_offset > self.character_count_before
            || self
                .caret_offset_after
                .is_some_and(|offset| offset > self.character_count_after)
            || self.postcondition_satisfied == Some(false)
        {
            return Err(ClipboardValidationError::TextEvidence);
        }
        Ok(())
    }
}

/// Content-free text insertion result metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TextInsertEvidence {
    /// Concrete strategy selected after policy evaluation.
    pub selected_strategy: TextStrategy,
    /// Exact source UTF-8 bytes.
    #[schemars(range(max = MAX_TEXT_INSERT_BYTES))]
    pub utf8_bytes: u64,
    /// Source Unicode scalar values, not grapheme clusters.
    #[schemars(range(max = MAX_TEXT_INSERT_BYTES))]
    pub unicode_scalars: u64,
    /// Complete scalars delivered before success/failure/cancellation.
    #[schemars(range(max = MAX_TEXT_INSERT_BYTES))]
    pub completed_scalars: u64,
    /// Required exactly when clipboard strategy was selected.
    pub clipboard: Option<ClipboardPasteEvidence>,
    /// Required exactly when AT-SPI semantic insertion was selected.
    pub semantic: Option<SemanticTextInsertEvidence>,
}

impl TextInsertEvidence {
    /// Rejects impossible counts and missing/unexpected clipboard evidence.
    pub fn validate(&self) -> Result<(), ClipboardValidationError> {
        if self.utf8_bytes > MAX_TEXT_INSERT_BYTES
            || self.unicode_scalars > self.utf8_bytes
            || (self.utf8_bytes == 0) != (self.unicode_scalars == 0)
            || self.completed_scalars > self.unicode_scalars
            || self.selected_strategy == TextStrategy::Auto
            || (self.selected_strategy == TextStrategy::Clipboard) != self.clipboard.is_some()
            || (self.selected_strategy == TextStrategy::Semantic) != self.semantic.is_some()
        {
            return Err(ClipboardValidationError::TextEvidence);
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.validate()?;
        }
        if let Some(semantic) = &self.semantic {
            semantic.validate()?;
        }
        Ok(())
    }
}

fn validate_targets(targets: &[ClipboardTarget]) -> Result<(), ClipboardValidationError> {
    if targets.len() > MAX_CLIPBOARD_TARGETS {
        return Err(ClipboardValidationError::TooManyTargets);
    }
    let mut unique = BTreeSet::new();
    for target in targets {
        ClipboardTarget::new(target.as_str())?;
        if !unique.insert(target) {
            return Err(ClipboardValidationError::DuplicateTarget);
        }
    }
    Ok(())
}

fn decoded_base64_length(value: &str) -> Result<usize, ClipboardValidationError> {
    if value.is_empty() {
        return Ok(0);
    }
    if !value.len().is_multiple_of(4)
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')))
    {
        return Err(ClipboardValidationError::Base64);
    }
    let padding = value.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2
        || value.as_bytes()[..value.len() - padding].contains(&b'=')
        || (padding == 1 && value.as_bytes()[value.len() - 2] == b'=')
    {
        return Err(ClipboardValidationError::Base64);
    }
    let bytes = value.as_bytes();
    if (padding == 2 && base64_value(bytes[bytes.len() - 3])? & 0x0f != 0)
        || (padding == 1 && base64_value(bytes[bytes.len() - 2])? & 0x03 != 0)
    {
        return Err(ClipboardValidationError::Base64);
    }
    value
        .len()
        .checked_div(4)
        .and_then(|groups| groups.checked_mul(3))
        .and_then(|bytes| bytes.checked_sub(padding))
        .ok_or(ClipboardValidationError::Base64)
}

fn base64_sha256(value: &str) -> Result<Sha256Digest, ClipboardValidationError> {
    let expected_length = decoded_base64_length(value)?;
    let mut hasher = Sha256::new();
    let mut produced = 0_usize;
    for group in value.as_bytes().chunks_exact(4) {
        let first = base64_value(group[0])?;
        let second = base64_value(group[1])?;
        let third = if group[2] == b'=' {
            0
        } else {
            base64_value(group[2])?
        };
        let fourth = if group[3] == b'=' {
            0
        } else {
            base64_value(group[3])?
        };
        let decoded = [
            (first << 2) | (second >> 4),
            (second << 4) | (third >> 2),
            (third << 6) | fourth,
        ];
        let remaining = expected_length.saturating_sub(produced).min(3);
        hasher.update(&decoded[..remaining]);
        produced += remaining;
    }
    if produced != expected_length {
        return Err(ClipboardValidationError::Base64);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut encoded = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Sha256Digest::new(encoded).map_err(|_| ClipboardValidationError::InlinePayload)
}

fn sha256_bytes(value: &[u8]) -> Result<Sha256Digest, ClipboardValidationError> {
    let digest: [u8; 32] = Sha256::digest(value).into();
    let mut encoded = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Sha256Digest::new(encoded).map_err(|_| ClipboardValidationError::InlinePayload)
}

fn validate_artifact_scope(
    artifact: &ArtifactRef,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), ClipboardValidationError> {
    if artifact.desktop_id != desktop_id || artifact.desktop_generation != desktop_generation {
        return Err(ClipboardValidationError::ReferenceScope);
    }
    Ok(())
}

fn base64_value(byte: u8) -> Result<u8, ClipboardValidationError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(ClipboardValidationError::Base64),
    }
}

/// Invalid clipboard, selection-transfer, or text metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClipboardValidationError {
    /// Target name is empty, unsafe, or too long.
    #[error("clipboard target is invalid")]
    Target,
    /// Inline payload exceeds its bound or contradicts its declared length.
    #[error("inline clipboard payload is invalid")]
    InlinePayload,
    /// Base64 alphabet or padding is invalid.
    #[error("inline clipboard base64 is invalid")]
    Base64,
    /// Artifact purpose, media type, length, or identity is invalid.
    #[error("clipboard artifact reference is invalid")]
    Artifact,
    /// Preferred/requested target count exceeds the bound.
    #[error("clipboard target list exceeds its bound")]
    TooManyTargets,
    /// Target lists must not repeat an atom.
    #[error("clipboard target list contains a duplicate")]
    DuplicateTarget,
    /// A transfer exceeded the selection ceiling.
    #[error("clipboard selection exceeds its byte ceiling")]
    SelectionTooLarge,
    /// Direct/INCR evidence is contradictory or incomplete.
    #[error("selection transfer evidence is invalid")]
    TransferEvidence,
    /// Actor-local revisions are positive.
    #[error("clipboard revision is invalid")]
    Revision,
    /// Restoration evidence overclaims what X11 can restore.
    #[error("clipboard restoration evidence is invalid")]
    RestorationEvidence,
    /// Paste request/transfer evidence is contradictory.
    #[error("clipboard paste evidence is invalid")]
    PasteEvidence,
    /// Paste observation timeout is outside the release-four bound.
    #[error("clipboard paste observation timeout is invalid")]
    PasteTimeout,
    /// Text source exceeds its policy ceiling.
    #[error("text insertion source exceeds its byte ceiling")]
    TextTooLarge,
    /// Text counts or strategy-specific evidence is contradictory.
    #[error("text insertion evidence is invalid")]
    TextEvidence,
    /// Strategy and clipboard-option presence are inconsistent.
    #[error("text insertion options are inconsistent with the strategy")]
    TextOptions,
    /// Semantic insertion options are invalid.
    #[error("semantic text insertion options are invalid")]
    SemanticOptions,
    /// Auto strategy order, bounds, or fallback policy is invalid.
    #[error("automatic text insertion policy is invalid")]
    AutoPolicy,
    /// The generation-bound text target is invalid.
    #[error("text insertion target is invalid")]
    TextTarget,
    /// A window or artifact reference belongs to another desktop lifetime.
    #[error("clipboard reference belongs to another desktop lifetime")]
    ReferenceScope,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command;

    fn digest() -> Result<Sha256Digest, crate::ArtifactValidationError> {
        Sha256Digest::new("00".repeat(32))
    }

    fn window_reference(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<WindowRef, crate::WindowValidationError> {
        Ok(WindowRef {
            desktop_id,
            desktop_generation,
            xid: 42,
            observed_generation: 1,
            identity_hash: crate::WindowIdentityHash::new("a".repeat(64))?,
        })
    }

    fn element_reference(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<ElementRef, crate::AccessibilityValidationError> {
        let atspi_generation = crate::AtspiGeneration::new(1)?;
        Ok(ElementRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            application: crate::ApplicationRef {
                desktop_id,
                desktop_generation,
                atspi_generation,
                unique_bus_name: crate::AtspiBusName::new(":1.42")?,
                root_object_path: crate::AtspiObjectPath::new("/org/a11y/atspi/accessible/root")?,
                app_instance_generation: 1,
                identity_hash: crate::AccessibilityIdentityHash::new("b".repeat(64))?,
            },
            object_path: crate::AtspiObjectPath::new("/org/a11y/atspi/accessible/42")?,
            object_identity_hash: crate::AccessibilityIdentityHash::new("c".repeat(64))?,
            cache_sequence: 7,
        })
    }

    fn semantic_options() -> SemanticTextInsertOptions {
        SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Caret,
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: true,
            postcondition: None,
        }
    }

    #[test]
    fn inline_text_is_serializable_but_redacted_from_debug()
    -> Result<(), Box<dyn std::error::Error>> {
        let canary = "CLIPBOARD_SECRET_CANARY";
        let value = SecretInlineText::new(canary)?;
        assert!(serde_json::to_string(&value)?.contains(canary));
        assert!(!format!("{value:?}").contains(canary));
        Ok(())
    }

    #[test]
    fn base64_length_and_diagnostics_are_checked() -> Result<(), Box<dyn std::error::Error>> {
        let value = SecretInlineBinary::new("c2VjcmV0", 6, base64_sha256("c2VjcmV0")?)?;
        assert!(!format!("{value:?}").contains("c2VjcmV0"));
        assert_eq!(value.decoded_length(), 6);
        assert_eq!(
            SecretInlineBinary::new("c2VjcmV0", 5, digest()?),
            Err(ClipboardValidationError::InlinePayload)
        );
        assert_eq!(
            SecretInlineBinary::new("c2VjcmV0", 6, digest()?),
            Err(ClipboardValidationError::InlinePayload)
        );
        Ok(())
    }

    #[test]
    fn target_lists_are_bounded_and_unique() -> Result<(), ClipboardValidationError> {
        let utf8 = ClipboardTarget::new("UTF8_STRING")?;
        assert_eq!(
            validate_targets(&[utf8.clone(), utf8]),
            Err(ClipboardValidationError::DuplicateTarget)
        );
        assert_eq!(
            ClipboardTarget::new("XENOTEER_ATTACKER_CREATED_ATOM"),
            Err(ClipboardValidationError::Target)
        );
        Ok(())
    }

    #[test]
    fn incr_requires_content_chunks_and_terminal_marker() -> Result<(), Box<dyn std::error::Error>>
    {
        let evidence = SelectionTransferEvidence {
            target: ClipboardTarget::new("UTF8_STRING")?,
            transfer: SelectionTransferMode::Incr {
                announced_minimum_bytes: 262_145,
                chunks: 4,
            },
            content_length: 200_000,
            sha256: digest()?,
            owner_changed: false,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Completed,
        };
        assert_eq!(
            evidence.validate(),
            Err(ClipboardValidationError::TransferEvidence)
        );
        Ok(())
    }

    #[test]
    fn clipboard_read_binds_text_bytes_to_transfer_digest() -> Result<(), Box<dyn std::error::Error>>
    {
        let text = SecretInlineText::new("secret")?;
        let target = ClipboardTarget::new("UTF8_STRING")?;
        let valid = ClipboardReadResult {
            selection: SelectionName::Clipboard,
            revision: 1,
            evidence: SelectionTransferEvidence {
                target,
                transfer: SelectionTransferMode::Direct,
                content_length: text.byte_len() as u64,
                sha256: sha256_bytes(text.expose_secret().as_bytes())?,
                owner_changed: false,
                terminal_chunk_observed: false,
                terminal: SelectionTransferTerminal::Completed,
            },
            content: ClipboardReadDelivery::InlineText { text },
        };
        assert!(valid.validate().is_ok());

        let mut mismatched = valid;
        mismatched.evidence.sha256 = digest()?;
        assert_eq!(
            mismatched.validate(),
            Err(ClipboardValidationError::TransferEvidence)
        );
        Ok(())
    }

    #[test]
    fn observed_failed_incr_is_valid_partial_paste_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let target = ClipboardTarget::new("UTF8_STRING")?;
        let evidence = ClipboardPasteEvidence {
            request_observed: true,
            requested_targets: vec![target.clone()],
            transfer: Some(SelectionTransferEvidence {
                target,
                transfer: SelectionTransferMode::Incr {
                    announced_minimum_bytes: 1_024,
                    chunks: 2,
                },
                content_length: 512,
                sha256: sha256_bytes(&vec![b'x'; 512])?,
                owner_changed: false,
                terminal_chunk_observed: false,
                terminal: SelectionTransferTerminal::Failed {
                    reason: SelectionTransferFailureReason::Timeout,
                },
            }),
            postcondition_met: None,
            restoration: ClipboardRestorationEvidence {
                requested: false,
                previous_owner_existed: false,
                preserved_bytes: 0,
                kind: ClipboardRestorationKind::NotRequested,
            },
        };
        assert!(evidence.validate().is_ok());
        Ok(())
    }

    #[test]
    fn restoration_never_claims_original_owner_restoration() {
        assert!(
            ClipboardRestorationEvidence {
                requested: true,
                previous_owner_existed: true,
                preserved_bytes: 12,
                kind: ClipboardRestorationKind::ValueCopy,
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            ClipboardRestorationEvidence {
                requested: false,
                previous_owner_existed: true,
                preserved_bytes: 12,
                kind: ClipboardRestorationKind::NotRequested,
            }
            .validate(),
            Err(ClipboardValidationError::RestorationEvidence)
        );
    }

    #[test]
    fn clipboard_strategy_requires_clipboard_evidence() {
        let result = TextInsertEvidence {
            selected_strategy: TextStrategy::Clipboard,
            utf8_bytes: 3,
            unicode_scalars: 3,
            completed_scalars: 3,
            clipboard: None,
            semantic: None,
        };
        assert_eq!(
            result.validate(),
            Err(ClipboardValidationError::TextEvidence)
        );

        let impossible_counts = TextInsertEvidence {
            selected_strategy: TextStrategy::Physical,
            utf8_bytes: 1,
            unicode_scalars: 2,
            completed_scalars: 0,
            clipboard: None,
            semantic: None,
        };
        assert_eq!(
            impossible_counts.validate(),
            Err(ClipboardValidationError::TextEvidence)
        );
    }

    #[test]
    fn text_target_context_rejects_another_desktop() -> Result<(), Box<dyn std::error::Error>> {
        let route_desktop = DesktopId::new();
        let route_generation = DesktopGeneration::new();
        let command = TextInsertCommand {
            text: TextSource::Inline {
                text: SecretInlineText::new("x")?,
            },
            target: TextTarget::Window {
                window: WindowRef {
                    desktop_id: DesktopId::new(),
                    desktop_generation: route_generation,
                    xid: 7,
                    observed_generation: 1,
                    identity_hash: crate::WindowIdentityHash::new("a".repeat(64))?,
                },
            },
            strategy: TextStrategy::Physical,
            clipboard_options: None,
            semantic_options: None,
            auto_policy: None,
        };
        assert_eq!(
            command.validate_for_desktop(route_desktop, route_generation),
            Err(ClipboardValidationError::ReferenceScope)
        );
        Ok(())
    }

    #[test]
    fn legacy_window_auto_shape_remains_valid_and_inspectable()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let value = serde_json::json!({
            "text": { "source": "inline", "text": "secret" },
            "target": {
                "target": "window",
                "window": window_reference(desktop_id, desktop_generation)?
            },
            "strategy": "auto",
            "clipboard_options": {
                "preserve_clipboard": false,
                "paste_observation_timeout_ms": 1000
            }
        });
        let command: TextInsertCommand = serde_json::from_value(value)?;
        assert!(command.validate().is_ok());
        assert_eq!(
            command.strategy_union(),
            TextStrategyUnion {
                semantic: false,
                physical: true,
                clipboard: true,
                physical_extended: true,
            }
        );
        assert!(command.requires_control_lease());
        Ok(())
    }

    #[test]
    fn auto_policy_is_nonempty_concrete_and_canonically_ordered() {
        let policy = |allowed_strategies| TextAutoPolicy {
            allowed_strategies,
            fallback: TextAutoFallbackPolicy::BeforeEffectOnly,
        };
        assert!(
            policy(vec![
                TextStrategy::Semantic,
                TextStrategy::Physical,
                TextStrategy::Clipboard,
                TextStrategy::PhysicalExtended,
            ])
            .validate()
            .is_ok()
        );
        for invalid in [
            Vec::new(),
            vec![TextStrategy::Physical, TextStrategy::Semantic],
            vec![TextStrategy::Semantic, TextStrategy::Semantic],
            vec![TextStrategy::Auto],
        ] {
            assert_eq!(
                policy(invalid).validate(),
                Err(ClipboardValidationError::AutoPolicy)
            );
        }
    }

    #[test]
    fn target_strategy_and_option_matrix_rejects_impossible_combinations()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let element = element_reference(desktop_id, desktop_generation)?;
        let window = window_reference(desktop_id, desktop_generation)?;
        let source = || {
            Ok::<_, ClipboardValidationError>(TextSource::Inline {
                text: SecretInlineText::new("secret")?,
            })
        };
        let semantic = TextInsertCommand {
            text: source()?,
            target: TextTarget::Element {
                element: Box::new(element.clone()),
                window_fallback: None,
            },
            strategy: TextStrategy::Semantic,
            clipboard_options: None,
            semantic_options: Some(semantic_options()),
            auto_policy: None,
        };
        assert!(semantic.validate().is_ok());
        let semantic_command = Command::TextInsert(semantic.clone());
        assert!(!semantic_command.requires_control_lease());
        assert!(
            crate::CommandEnvelope::new(
                crate::ProtocolVersion::V1_0,
                crate::RequestId::new(),
                crate::CommandId::new(),
                desktop_id,
                desktop_generation,
                semantic_command,
            )
            .is_ok()
        );
        assert!(semantic.strategy_union().requires_accessibility_write());

        let semantic_window = TextInsertCommand {
            text: source()?,
            target: TextTarget::Window {
                window: window.clone(),
            },
            ..semantic.clone()
        };
        assert_eq!(
            semantic_window.validate(),
            Err(ClipboardValidationError::TextTarget)
        );

        let physical_without_window = TextInsertCommand {
            text: source()?,
            target: TextTarget::Element {
                element: Box::new(element.clone()),
                window_fallback: None,
            },
            strategy: TextStrategy::Physical,
            clipboard_options: None,
            semantic_options: None,
            auto_policy: None,
        };
        assert_eq!(
            physical_without_window.validate(),
            Err(ClipboardValidationError::TextTarget)
        );

        let semantic_auto = TextInsertCommand {
            text: source()?,
            target: TextTarget::Element {
                element: Box::new(element.clone()),
                window_fallback: None,
            },
            strategy: TextStrategy::Auto,
            clipboard_options: None,
            semantic_options: Some(semantic_options()),
            auto_policy: Some(TextAutoPolicy {
                allowed_strategies: vec![TextStrategy::Semantic],
                fallback: TextAutoFallbackPolicy::BeforeEffectOnly,
            }),
        };
        assert!(semantic_auto.validate().is_ok());
        assert!(!Command::TextInsert(semantic_auto).requires_control_lease());

        let union_auto = TextInsertCommand {
            text: source()?,
            target: TextTarget::Element {
                element: Box::new(element),
                window_fallback: Some(window),
            },
            strategy: TextStrategy::Auto,
            clipboard_options: None,
            semantic_options: Some(semantic_options()),
            auto_policy: Some(TextAutoPolicy {
                allowed_strategies: vec![TextStrategy::Semantic, TextStrategy::Physical],
                fallback: TextAutoFallbackPolicy::BeforeEffectOnly,
            }),
        };
        assert!(union_auto.validate().is_ok());
        let union_auto = Command::TextInsert(union_auto);
        assert!(union_auto.requires_control_lease());
        assert_eq!(
            crate::CommandEnvelope::new(
                crate::ProtocolVersion::V1_0,
                crate::RequestId::new(),
                crate::CommandId::new(),
                desktop_id,
                desktop_generation,
                union_auto,
            ),
            Err(crate::EnvelopeValidationError::LeaseRequired)
        );

        let mut missing_policy = semantic.clone();
        missing_policy.strategy = TextStrategy::Auto;
        assert_eq!(
            missing_policy.validate(),
            Err(ClipboardValidationError::AutoPolicy)
        );
        Ok(())
    }

    #[test]
    fn semantic_options_have_no_protected_write_bypass() {
        let value = serde_json::json!({
            "insertion_point": { "kind": "caret" },
            "selection": "collapse_after",
            "verify_length_only": true,
            "postcondition": null,
            "allow_protected": true
        });
        assert!(serde_json::from_value::<SemanticTextInsertOptions>(value).is_err());
    }

    #[test]
    fn semantic_evidence_is_concrete_content_free_and_generation_fenced()
    -> Result<(), Box<dyn std::error::Error>> {
        let semantic = SemanticTextInsertEvidence {
            element: element_reference(DesktopId::new(), DesktopGeneration::new())?,
            revision_before: AccessibilityRevision::new(4)?,
            revision_after: AccessibilityRevision::new(5)?,
            backend_accepted: true,
            insertion_offset: 2,
            character_count_before: 4,
            character_count_after: 7,
            caret_offset_after: Some(5),
            selection_count_after: Some(0),
            verified_length_only: true,
            postcondition_satisfied: Some(true),
        };
        let evidence = TextInsertEvidence {
            selected_strategy: TextStrategy::Semantic,
            utf8_bytes: 3,
            unicode_scalars: 3,
            completed_scalars: 3,
            clipboard: None,
            semantic: Some(semantic.clone()),
        };
        assert!(evidence.validate().is_ok());
        let encoded = serde_json::to_string(&evidence)?;
        assert!(!encoded.contains("text"));

        let mut rejected = evidence.clone();
        rejected.selected_strategy = TextStrategy::Auto;
        assert_eq!(
            rejected.validate(),
            Err(ClipboardValidationError::TextEvidence)
        );
        let mut rejected = evidence;
        rejected.semantic = None;
        assert_eq!(
            rejected.validate(),
            Err(ClipboardValidationError::TextEvidence)
        );
        let mut rejected = semantic;
        rejected.revision_after = AccessibilityRevision::new(3)?;
        assert_eq!(
            rejected.validate(),
            Err(ClipboardValidationError::TextEvidence)
        );
        Ok(())
    }
}
