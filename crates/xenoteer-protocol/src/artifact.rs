//! Purpose-bound references for immutable private artifacts.

use core::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{ArtifactId, DesktopGeneration, DesktopId, Timestamp};

/// Maximum accepted media-type bytes.
pub const MAX_ARTIFACT_CONTENT_TYPE_BYTES: usize = 128;
/// Absolute release-one artifact object ceiling.
pub const MAX_ARTIFACT_BYTES: u64 = 32 * 1_024 * 1_024;
/// Clipboard artifacts have the stricter X11 selection ceiling.
pub const MAX_CLIPBOARD_ARTIFACT_BYTES: u64 = 16 * 1_024 * 1_024;

/// Why an immutable object exists and which capability authorizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPurpose {
    /// Private caller upload consumed only as clipboard command input.
    ClipboardInput,
    /// Private output produced by a clipboard read.
    ClipboardOutput,
    /// Screenshot bytes produced by an explicit capture request.
    Screenshot,
    /// Bounded action-trace evidence.
    ActionTrace,
    /// Explicitly requested diagnostic support material.
    SupportBundle,
}

impl ArtifactPurpose {
    /// Returns the immutable byte ceiling for this purpose.
    #[must_use]
    pub const fn maximum_bytes(self) -> u64 {
        match self {
            Self::ClipboardInput | Self::ClipboardOutput => MAX_CLIPBOARD_ARTIFACT_BYTES,
            Self::Screenshot | Self::ActionTrace | Self::SupportBundle => MAX_ARTIFACT_BYTES,
        }
    }
}

/// A validated HTTP media type without attacker-controlled control characters.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "content_type_schema")]
pub struct ArtifactContentType(String);

fn content_type_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 3,
        "maxLength": MAX_ARTIFACT_CONTENT_TYPE_BYTES,
        "pattern": "^[!#$&^_.+A-Za-z0-9-]+/[!#$&^_.+A-Za-z0-9-]+(?:;[ -~]+)?$"
    })
}

impl ArtifactContentType {
    /// Creates a bounded syntactically conservative media type.
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactValidationError> {
        let value = value.into();
        validate_content_type(&value)?;
        Ok(Self(value))
    }

    /// Returns the exact validated media type.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ArtifactContentType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ArtifactContentType")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for ArtifactContentType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactContentType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Canonical lowercase hexadecimal SHA-256 content identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "sha256_schema")]
pub struct Sha256Digest(String);

fn sha256_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 64,
        "maxLength": 64,
        "pattern": "^[0-9a-f]{64}$"
    })
}

impl Sha256Digest {
    /// Creates a canonical lowercase hexadecimal digest.
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactValidationError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ArtifactValidationError::Sha256);
        }
        Ok(Self(value))
    }

    /// Returns canonical lowercase hexadecimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sha256Digest([REDACTED])")
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// An immutable artifact reference; possession alone never grants access.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRef {
    /// Opaque object identifier.
    pub artifact_id: ArtifactId,
    /// Capability-defining object purpose.
    pub purpose: ArtifactPurpose,
    /// Desktop owning the object.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the object.
    pub desktop_generation: DesktopGeneration,
    /// Validated response media type.
    pub content_type: ArtifactContentType,
    /// Exact immutable body length.
    #[schemars(range(min = 1, max = MAX_ARTIFACT_BYTES))]
    pub content_length: u64,
    /// Digest rechecked whenever an artifact is consumed by a command.
    pub sha256: Sha256Digest,
    /// Publication time.
    pub created_at: Timestamp,
    /// Time after which the object is no longer usable.
    pub expires_at: Timestamp,
}

/// Request-direction representation of [`ArtifactRef`].
///
/// Artifact responses permit additive metadata, while commands consuming an
/// artifact must reject every unrecognized authority-bearing field.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "ArtifactRef")]
pub(crate) struct StrictArtifactRef {
    artifact_id: ArtifactId,
    purpose: ArtifactPurpose,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    content_type: ArtifactContentType,
    #[schemars(range(min = 1, max = MAX_ARTIFACT_BYTES))]
    content_length: u64,
    sha256: Sha256Digest,
    created_at: Timestamp,
    expires_at: Timestamp,
}

impl From<StrictArtifactRef> for ArtifactRef {
    fn from(value: StrictArtifactRef) -> Self {
        Self {
            artifact_id: value.artifact_id,
            purpose: value.purpose,
            desktop_id: value.desktop_id,
            desktop_generation: value.desktop_generation,
            content_type: value.content_type,
            content_length: value.content_length,
            sha256: value.sha256,
            created_at: value.created_at,
            expires_at: value.expires_at,
        }
    }
}

pub(crate) fn deserialize_strict_artifact_ref<'de, D>(
    deserializer: D,
) -> Result<ArtifactRef, D::Error>
where
    D: Deserializer<'de>,
{
    StrictArtifactRef::deserialize(deserializer).map(Into::into)
}

impl ArtifactRef {
    /// Revalidates identity, purpose limit, and positive retention interval.
    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        if self.artifact_id.as_uuid().is_nil()
            || self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
        {
            return Err(ArtifactValidationError::NilIdentifier);
        }
        validate_content_type(self.content_type.as_str())?;
        Sha256Digest::new(self.sha256.as_str())?;
        if self.content_length == 0 || self.content_length > self.purpose.maximum_bytes() {
            return Err(ArtifactValidationError::ContentLength);
        }
        if self.expires_at <= self.created_at {
            return Err(ArtifactValidationError::Retention);
        }
        Ok(())
    }
}

impl fmt::Debug for ArtifactRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactRef")
            .field("artifact_id", &self.artifact_id)
            .field("purpose", &self.purpose)
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .field("content_type", &self.content_type)
            .field("content_length", &self.content_length)
            .field("sha256", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

fn validate_content_type(value: &str) -> Result<(), ArtifactValidationError> {
    if value.len() < 3
        || value.len() > MAX_ARTIFACT_CONTENT_TYPE_BYTES
        || value.trim() != value
        || value.ends_with(';')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(ArtifactValidationError::ContentType);
    }
    let essence = value.split_once(';').map_or(value, |(essence, _)| essence);
    let Some((type_, subtype)) = essence.split_once('/') else {
        return Err(ArtifactValidationError::ContentType);
    };
    if type_.is_empty()
        || subtype.is_empty()
        || subtype.contains('/')
        || !type_.bytes().all(is_media_token_byte)
        || !subtype.bytes().all(is_media_token_byte)
    {
        return Err(ArtifactValidationError::ContentType);
    }
    Ok(())
}

const fn is_media_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
        )
}

/// Invalid artifact wire metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ArtifactValidationError {
    /// A public identifier is nil.
    #[error("artifact reference contains a nil identifier")]
    NilIdentifier,
    /// The media type is empty, malformed, contains unsafe bytes, or is too long.
    #[error("artifact content type is invalid")]
    ContentType,
    /// The SHA-256 value is not canonical lowercase hexadecimal.
    #[error("artifact SHA-256 is invalid")]
    Sha256,
    /// The object length is zero or exceeds its purpose limit.
    #[error("artifact content length is outside its purpose bound")]
    ContentLength,
    /// Expiry does not follow creation.
    #[error("artifact retention interval is invalid")]
    Retention,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> Result<Sha256Digest, ArtifactValidationError> {
        Sha256Digest::new("ab".repeat(32))
    }

    fn reference(
        purpose: ArtifactPurpose,
        length: u64,
    ) -> Result<ArtifactRef, Box<dyn std::error::Error>> {
        Ok(ArtifactRef {
            artifact_id: ArtifactId::new(),
            purpose,
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            content_type: ArtifactContentType::new("application/octet-stream")?,
            content_length: length,
            sha256: digest()?,
            created_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
            expires_at: Timestamp::parse("2026-07-21T01:00:00Z")?,
        })
    }

    #[test]
    fn purpose_specific_size_bound_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            reference(
                ArtifactPurpose::ClipboardInput,
                MAX_CLIPBOARD_ARTIFACT_BYTES
            )?
            .validate()
            .is_ok()
        );
        assert_eq!(
            reference(
                ArtifactPurpose::ClipboardInput,
                MAX_CLIPBOARD_ARTIFACT_BYTES + 1
            )?
            .validate(),
            Err(ArtifactValidationError::ContentLength)
        );
        assert!(
            reference(ArtifactPurpose::Screenshot, MAX_ARTIFACT_BYTES)?
                .validate()
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn digest_is_wire_visible_but_redacted_from_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = reference(ArtifactPurpose::Screenshot, 4)?;
        let encoded = serde_json::to_string(&value)?;
        assert!(encoded.contains(value.sha256.as_str()));
        assert!(!format!("{value:?}").contains(value.sha256.as_str()));
        Ok(())
    }

    #[test]
    fn media_type_rejects_controls_and_missing_subtype() {
        assert!(ArtifactContentType::new("image/png").is_ok());
        assert!(ArtifactContentType::new("text/plain;charset=utf-8").is_ok());
        assert_eq!(
            ArtifactContentType::new("image"),
            Err(ArtifactValidationError::ContentType)
        );
        assert_eq!(
            ArtifactContentType::new("text/plain\nsecret"),
            Err(ArtifactValidationError::ContentType)
        );
    }
}
