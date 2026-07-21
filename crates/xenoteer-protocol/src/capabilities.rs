//! Capability discovery types.

use core::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// Maximum UTF-8 byte length of a capability identifier.
pub const MAX_CAPABILITY_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a stable capability reason code.
pub const MAX_CAPABILITY_REASON_CODE_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a disclosed backend version.
pub const MAX_BACKEND_VERSION_BYTES: usize = 128;
/// Maximum capabilities carried by one discovery report.
pub const MAX_CAPABILITIES: usize = 256;

/// A stable dotted capability identifier such as `input.pointer.smooth`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "capability_id_schema")]
pub struct CapabilityId(String);

fn capability_id_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_CAPABILITY_ID_BYTES,
        "pattern": "^[a-z0-9_-]+(?:\\.[a-z0-9_-]+)*$"
    })
}

impl CapabilityId {
    /// Creates a checked capability identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, CapabilityIdError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_CAPABILITY_ID_BYTES {
            return Err(CapabilityIdError::Length);
        }
        if value.split('.').any(str::is_empty)
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        {
            return Err(CapabilityIdError::Characters);
        }
        Ok(Self(value))
    }

    /// Returns the stable identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CapabilityId")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for CapabilityId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapabilityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Capability identifier validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CapabilityIdError {
    /// The identifier is empty or exceeds the protocol limit.
    #[error("capability identifier has invalid length")]
    Length,
    /// The identifier contains invalid characters or empty dotted segments.
    #[error("capability identifier must use lowercase dotted ASCII segments")]
    Characters,
}

/// Runtime availability of one advertised capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// The capability passed its probe and can accept work.
    Available,
    /// The capability works only partially or under a known limitation.
    Degraded,
    /// The backend or prerequisite is currently unavailable.
    Unavailable,
    /// Configuration or policy disabled the capability.
    Disabled,
}

/// One capability and its current safe status summary.
///
/// JSON Schema string lengths count Unicode code points; admission additionally
/// applies the documented UTF-8 byte ceilings before values are exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capability {
    id: CapabilityId,
    status: CapabilityStatus,
    #[schemars(
        length(min = 1, max = MAX_CAPABILITY_REASON_CODE_BYTES),
        regex(pattern = "^[a-z0-9._-]+$")
    )]
    reason_code: Option<String>,
    #[schemars(length(min = 1, max = MAX_BACKEND_VERSION_BYTES))]
    backend_version: Option<String>,
}

impl Capability {
    /// Creates a capability without optional backend disclosure fields.
    #[must_use]
    pub const fn new(id: CapabilityId, status: CapabilityStatus) -> Self {
        Self {
            id,
            status,
            reason_code: None,
            backend_version: None,
        }
    }

    /// Adds a checked stable reason code.
    pub fn with_reason_code(
        mut self,
        reason_code: impl Into<String>,
    ) -> Result<Self, CapabilityValidationError> {
        self.reason_code = Some(reason_code.into());
        self.validate()?;
        Ok(self)
    }

    /// Adds a checked, safe backend version string.
    pub fn with_backend_version(
        mut self,
        backend_version: impl Into<String>,
    ) -> Result<Self, CapabilityValidationError> {
        self.backend_version = Some(backend_version.into());
        self.validate()?;
        Ok(self)
    }

    /// Validates bounded values obtained through deserialization.
    pub fn validate(&self) -> Result<(), CapabilityValidationError> {
        if self.reason_code.as_deref().is_some_and(|reason| {
            reason.is_empty()
                || reason.len() > MAX_CAPABILITY_REASON_CODE_BYTES
                || !reason.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                })
        }) {
            return Err(CapabilityValidationError::ReasonCode);
        }
        if self.backend_version.as_deref().is_some_and(|version| {
            version.is_empty()
                || version.len() > MAX_BACKEND_VERSION_BYTES
                || version.chars().any(char::is_control)
        }) {
            return Err(CapabilityValidationError::BackendVersion);
        }
        Ok(())
    }

    /// Returns the stable identifier.
    #[must_use]
    pub const fn id(&self) -> &CapabilityId {
        &self.id
    }

    /// Returns current capability status.
    #[must_use]
    pub const fn status(&self) -> CapabilityStatus {
        self.status
    }

    /// Returns the optional stable reason code.
    #[must_use]
    pub fn reason_code(&self) -> Option<&str> {
        self.reason_code.as_deref()
    }

    /// Returns the optional safe backend version.
    #[must_use]
    pub fn backend_version(&self) -> Option<&str> {
        self.backend_version.as_deref()
    }
}

/// Capability discovery response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityReport {
    #[schemars(length(max = MAX_CAPABILITIES))]
    capabilities: Vec<Capability>,
}

impl CapabilityReport {
    /// Sorts and rejects duplicate capability identifiers.
    pub fn checked(mut capabilities: Vec<Capability>) -> Result<Self, CapabilityReportError> {
        if capabilities.len() > MAX_CAPABILITIES {
            return Err(CapabilityReportError::Limit);
        }
        for capability in &capabilities {
            capability.validate()?;
        }
        capabilities.sort_by(|left, right| left.id.cmp(&right.id));
        if capabilities.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(CapabilityReportError::Duplicate);
        }
        Ok(Self { capabilities })
    }

    /// Validates and sorts a report obtained through deserialization.
    pub fn validate(&mut self) -> Result<(), CapabilityReportError> {
        if self.capabilities.len() > MAX_CAPABILITIES {
            return Err(CapabilityReportError::Limit);
        }
        for capability in &self.capabilities {
            capability.validate()?;
        }
        self.capabilities
            .sort_by(|left, right| left.id.cmp(&right.id));
        if self
            .capabilities
            .windows(2)
            .any(|pair| pair[0].id == pair[1].id)
        {
            return Err(CapabilityReportError::Duplicate);
        }
        Ok(())
    }

    /// Returns capabilities in deterministic identifier order after validation.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }
}

/// Capability output validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CapabilityValidationError {
    /// Reason code is empty, too long, or not stable lowercase ASCII.
    #[error("capability reason code is invalid")]
    ReasonCode,
    /// Backend version is empty, too long, or contains control characters.
    #[error("capability backend version is invalid")]
    BackendVersion,
}

/// Capability-report construction or validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CapabilityReportError {
    /// The report contains more capabilities than the protocol permits.
    #[error("capability report exceeds its entry limit")]
    Limit,
    /// One capability contains invalid bounded output.
    #[error(transparent)]
    Invalid(#[from] CapabilityValidationError),
    /// More than one entry has the same identifier.
    #[error("capability report contains a duplicate identifier")]
    Duplicate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_capability_names() {
        assert!(CapabilityId::new("input.pointer.smooth").is_ok());
        assert_eq!(
            CapabilityId::new("Input..Pointer"),
            Err(CapabilityIdError::Characters)
        );
    }

    #[test]
    fn rejects_unbounded_capability_output() -> Result<(), CapabilityIdError> {
        let id = CapabilityId::new("input.pointer.smooth")?;
        let capability = Capability {
            id,
            status: CapabilityStatus::Degraded,
            reason_code: Some("x".repeat(MAX_CAPABILITY_REASON_CODE_BYTES + 1)),
            backend_version: None,
        };
        assert_eq!(
            capability.validate(),
            Err(CapabilityValidationError::ReasonCode)
        );
        Ok(())
    }

    #[test]
    fn capability_report_enforces_count_before_content_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = (0..=MAX_CAPABILITIES)
            .map(|index| {
                Ok(Capability::new(
                    CapabilityId::new(format!("capability.{index}"))?,
                    CapabilityStatus::Available,
                ))
            })
            .collect::<Result<Vec<_>, CapabilityIdError>>()?;

        assert!(CapabilityReport::checked(capabilities[..MAX_CAPABILITIES].to_vec()).is_ok());
        let mut excessive = capabilities.clone();
        excessive[0].reason_code = Some(String::new());
        assert_eq!(
            CapabilityReport::checked(excessive.clone()),
            Err(CapabilityReportError::Limit)
        );

        let mut deserialized_shape = CapabilityReport {
            capabilities: excessive,
        };
        assert_eq!(
            deserialized_shape.validate(),
            Err(CapabilityReportError::Limit)
        );
        Ok(())
    }
}
