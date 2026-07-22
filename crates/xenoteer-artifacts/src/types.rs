use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_OWNER_BYTES: usize = 256;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_POLICY_REVISION_BYTES: usize = 256;
const MAX_CONTENT_TYPE_BYTES: usize = 128;
const MAX_REDACTION_POLICY_BYTES: usize = 256;

/// An opaque, unguessable artifact identifier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ArtifactId(Uuid);

impl ArtifactId {
    pub(crate) fn random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the underlying UUID for protocol conversion.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Converts a non-nil UUID from the protocol boundary.
    pub fn from_uuid(value: Uuid) -> Result<Self, ValidationError> {
        if value.is_nil() {
            return Err(ValidationError::ArtifactId);
        }
        Ok(Self(value))
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl FromStr for ArtifactId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| ValidationError::ArtifactId)?;
        if uuid.is_nil() || value != uuid.hyphenated().to_string() {
            return Err(ValidationError::ArtifactId);
        }
        Ok(Self(uuid))
    }
}

/// Stable authenticated-principal identity used to scope an artifact.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ArtifactOwner(String);

impl ArtifactOwner {
    /// Creates a checked owner identity.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_text(&value, MAX_OWNER_BYTES, ValidationError::Owner)?;
        Ok(Self(value))
    }

    /// Returns the authenticated-principal identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        validate_text(&self.0, MAX_OWNER_BYTES, ValidationError::Owner)
    }
}

/// Desktop generation to which an artifact is bound.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DesktopGeneration(Uuid);

impl DesktopGeneration {
    /// Wraps a protocol desktop-generation UUID without adding a protocol-crate
    /// dependency.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the underlying UUID for protocol conversion.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TimestampMillis(u64);

impl TimestampMillis {
    /// Creates a timestamp from Unix-epoch milliseconds.
    pub const fn from_unix_millis(value: u64) -> Self {
        Self(value)
    }

    /// Returns Unix-epoch milliseconds.
    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_add(self, millis: u64) -> Option<Self> {
        self.0.checked_add(millis).map(Self)
    }
}

/// Purpose that determines the higher-layer authorization policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtifactPurpose {
    /// Caller-supplied bytes intended for a clipboard write.
    ClipboardInput,
    /// Clipboard bytes captured for an authorized reader.
    ClipboardOutput,
    /// A captured desktop image.
    Screenshot,
    /// An action or diagnostic trace.
    ActionTrace,
    /// An explicitly requested support bundle.
    SupportBundle,
}

impl ArtifactPurpose {
    pub(crate) const fn maximum_bytes(self) -> u64 {
        match self {
            Self::ClipboardInput | Self::ClipboardOutput => 16 * 1024 * 1024,
            Self::Screenshot | Self::ActionTrace | Self::SupportBundle => 32 * 1024 * 1024,
        }
    }
}

/// Non-secret authorization provenance recorded with an artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProvenance {
    capability: String,
    policy_revision: Option<String>,
}

impl CapabilityProvenance {
    /// Records the capability used to authorize creation and an optional policy
    /// revision. Token values and grant secrets must never be supplied here.
    pub fn new(
        capability: impl Into<String>,
        policy_revision: Option<String>,
    ) -> Result<Self, ValidationError> {
        let capability = capability.into();
        validate_capability(&capability)?;
        if let Some(revision) = &policy_revision {
            validate_text(
                revision,
                MAX_POLICY_REVISION_BYTES,
                ValidationError::PolicyRevision,
            )?;
        }
        Ok(Self {
            capability,
            policy_revision,
        })
    }

    /// Returns the capability name, such as `capture:read`.
    pub fn capability(&self) -> &str {
        &self.capability
    }

    /// Returns the non-secret policy revision, when recorded.
    pub fn policy_revision(&self) -> Option<&str> {
        self.policy_revision.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        validate_capability(&self.capability)?;
        if let Some(revision) = &self.policy_revision {
            validate_text(
                revision,
                MAX_POLICY_REVISION_BYTES,
                ValidationError::PolicyRevision,
            )?;
        }
        Ok(())
    }
}

/// Describes whether content redaction was applied before storage.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionMetadata {
    applied: bool,
    policy: Option<String>,
}

impl RedactionMetadata {
    /// Records the redaction decision and an optional non-secret policy name.
    pub fn new(applied: bool, policy: Option<String>) -> Result<Self, ValidationError> {
        if let Some(policy) = &policy {
            validate_text(
                policy,
                MAX_REDACTION_POLICY_BYTES,
                ValidationError::RedactionPolicy,
            )?;
        }
        Ok(Self { applied, policy })
    }

    /// Returns whether redaction was applied.
    pub const fn applied(&self) -> bool {
        self.applied
    }

    /// Returns the redaction policy name, when recorded.
    pub fn policy(&self) -> Option<&str> {
        self.policy.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        if let Some(policy) = &self.policy {
            validate_text(
                policy,
                MAX_REDACTION_POLICY_BYTES,
                ValidationError::RedactionPolicy,
            )?;
        }
        Ok(())
    }
}

/// SHA-256 content digest.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// Creates a digest from its 32 raw bytes.
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Returns the raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sha256Digest([REDACTED])")
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Sha256Digest {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ValidationError::Sha256);
        }
        let mut bytes = [0_u8; 32];
        for (index, output) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *output = u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| ValidationError::Sha256)?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Owner and desktop-generation scope required for every lookup and delete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactScope {
    owner: ArtifactOwner,
    desktop_generation: DesktopGeneration,
    purpose: ArtifactPurpose,
    provenance: CapabilityProvenance,
}

impl ArtifactScope {
    /// Creates an artifact access scope from authenticated context and the
    /// exact purpose/provenance policy authorized for this operation.
    pub fn new(
        owner: ArtifactOwner,
        desktop_generation: DesktopGeneration,
        purpose: ArtifactPurpose,
        provenance: CapabilityProvenance,
    ) -> Self {
        Self {
            owner,
            desktop_generation,
            purpose,
            provenance,
        }
    }

    pub(crate) fn matches(&self, metadata: &ArtifactMetadata) -> bool {
        self.owner == metadata.owner
            && self.desktop_generation == metadata.desktop_generation
            && self.purpose == metadata.purpose
            && self.provenance == metadata.provenance
    }
}

/// Validated inputs for one immutable artifact creation.
#[derive(Clone, Debug)]
pub struct ArtifactCreate {
    pub(crate) owner: ArtifactOwner,
    pub(crate) purpose: ArtifactPurpose,
    pub(crate) desktop_generation: DesktopGeneration,
    pub(crate) provenance: CapabilityProvenance,
    pub(crate) content_type: String,
    pub(crate) expected_size: u64,
    pub(crate) expected_sha256: Option<Sha256Digest>,
    pub(crate) expires_at: TimestampMillis,
    pub(crate) redaction: RedactionMetadata,
}

impl ArtifactCreate {
    /// Creates a bounded artifact request. `expected_size` is mandatory so
    /// quota is reserved before the streaming body is read.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: ArtifactOwner,
        purpose: ArtifactPurpose,
        desktop_generation: DesktopGeneration,
        provenance: CapabilityProvenance,
        content_type: impl Into<String>,
        expected_size: u64,
        expires_at: TimestampMillis,
    ) -> Result<Self, ValidationError> {
        let content_type = content_type.into();
        validate_content_type(&content_type)?;
        Ok(Self {
            owner,
            purpose,
            desktop_generation,
            provenance,
            content_type,
            expected_size,
            expected_sha256: None,
            expires_at,
            redaction: RedactionMetadata::default(),
        })
    }

    /// Requires the streaming bytes to match the caller-provided digest.
    #[must_use]
    pub fn with_expected_sha256(mut self, expected: Sha256Digest) -> Self {
        self.expected_sha256 = Some(expected);
        self
    }

    /// Records redaction metadata supplied by the content-producing policy.
    #[must_use]
    pub fn with_redaction(mut self, redaction: RedactionMetadata) -> Self {
        self.redaction = redaction;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        self.owner.validate()?;
        if self.desktop_generation.as_uuid().is_nil() {
            return Err(ValidationError::DesktopGeneration);
        }
        self.provenance.validate()?;
        self.redaction.validate()?;
        validate_content_type(&self.content_type)
    }
}

/// Immutable metadata returned with an artifact reference or body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub(crate) id: ArtifactId,
    pub(crate) owner: ArtifactOwner,
    pub(crate) purpose: ArtifactPurpose,
    pub(crate) desktop_generation: DesktopGeneration,
    pub(crate) provenance: CapabilityProvenance,
    pub(crate) content_type: String,
    pub(crate) created_at: TimestampMillis,
    pub(crate) expires_at: TimestampMillis,
    pub(crate) size: u64,
    pub(crate) sha256: Sha256Digest,
    pub(crate) redaction: RedactionMetadata,
}

impl ArtifactMetadata {
    /// Returns the opaque artifact ID.
    pub const fn id(&self) -> ArtifactId {
        self.id
    }

    /// Returns the artifact owner.
    pub fn owner(&self) -> &ArtifactOwner {
        &self.owner
    }

    /// Returns the authorization-relevant purpose.
    pub const fn purpose(&self) -> ArtifactPurpose {
        self.purpose
    }

    /// Returns the bound desktop generation.
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns non-secret capability provenance.
    pub fn provenance(&self) -> &CapabilityProvenance {
        &self.provenance
    }

    /// Returns the stored media type.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Returns the durable publication timestamp.
    pub const fn created_at(&self) -> TimestampMillis {
        self.created_at
    }

    /// Returns the artifact expiry timestamp.
    pub const fn expires_at(&self) -> TimestampMillis {
        self.expires_at
    }

    /// Returns the immutable body length.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the server-computed body digest.
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// Returns the redaction decision.
    pub fn redaction(&self) -> &RedactionMetadata {
        &self.redaction
    }

    pub(crate) fn validate(&self, directory_id: ArtifactId) -> Result<(), ValidationError> {
        if self.id != directory_id {
            return Err(ValidationError::ArtifactId);
        }
        self.owner.validate()?;
        if self.desktop_generation.as_uuid().is_nil() {
            return Err(ValidationError::DesktopGeneration);
        }
        self.provenance.validate()?;
        self.redaction.validate()?;
        validate_content_type(&self.content_type)?;
        if self.created_at >= self.expires_at {
            return Err(ValidationError::Expiry);
        }
        if self.size == 0 || self.size > self.purpose.maximum_bytes() {
            return Err(ValidationError::BodySize);
        }
        Ok(())
    }
}

/// Invalid caller input or corrupt persisted metadata.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ValidationError {
    /// Artifact ID was not a canonical hyphenated UUID.
    #[error("artifact ID is not a canonical UUID")]
    ArtifactId,
    /// Owner identity was empty, too long, or contained control characters.
    #[error("artifact owner is invalid")]
    Owner,
    /// Desktop generation was the nil UUID.
    #[error("artifact desktop generation is invalid")]
    DesktopGeneration,
    /// Capability name did not use the bounded `namespace:action` form.
    #[error("artifact capability provenance is invalid")]
    Capability,
    /// Policy revision was empty, too long, or contained control characters.
    #[error("artifact policy revision is invalid")]
    PolicyRevision,
    /// Content type was empty, too long, or contained non-printable bytes.
    #[error("artifact content type is invalid")]
    ContentType,
    /// Redaction policy was empty, too long, or contained control characters.
    #[error("artifact redaction policy is invalid")]
    RedactionPolicy,
    /// SHA-256 digest was not 64 hexadecimal digits.
    #[error("artifact SHA-256 digest is invalid")]
    Sha256,
    /// Creation and expiry timestamps were inconsistent.
    #[error("artifact expiry is invalid")]
    Expiry,
    /// Body size was zero or exceeded its purpose-specific hard ceiling.
    #[error("artifact body size is invalid")]
    BodySize,
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    error: ValidationError,
) -> Result<(), ValidationError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(error);
    }
    Ok(())
}

fn validate_capability(value: &str) -> Result<(), ValidationError> {
    if value.is_empty()
        || value.len() > MAX_CAPABILITY_BYTES
        || value.starts_with(':')
        || value.ends_with(':')
        || !value.contains(':')
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_:-".contains(&byte)
        })
    {
        return Err(ValidationError::Capability);
    }
    Ok(())
}

fn validate_content_type(value: &str) -> Result<(), ValidationError> {
    if value.is_empty()
        || value.len() > MAX_CONTENT_TYPE_BYTES
        || value.trim() != value
        || value.ends_with(';')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(ValidationError::ContentType);
    }
    let essence = value.split_once(';').map_or(value, |(essence, _)| essence);
    let Some((type_, subtype)) = essence.split_once('/') else {
        return Err(ValidationError::ContentType);
    };
    if type_.is_empty()
        || subtype.is_empty()
        || subtype.contains('/')
        || !type_.bytes().all(is_media_token_byte)
        || !subtype.bytes().all(is_media_token_byte)
    {
        return Err(ValidationError::ContentType);
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
