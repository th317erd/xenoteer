//! Checked RFC 3339 UTC timestamps.

use core::{cmp::Ordering, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// A canonical RFC 3339 timestamp with an explicit UTC offset.
#[derive(Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(schema_with = "timestamp_schema")]
pub struct Timestamp(String);

fn timestamp_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "format": "date-time"
    })
}

impl Timestamp {
    /// Parses and canonicalizes an RFC 3339 timestamp to UTC.
    pub fn parse(value: &str) -> Result<Self, TimestampError> {
        let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| TimestampError)?;
        let canonical = parsed
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|_| TimestampError)?;
        Ok(Self(canonical))
    }

    /// Builds a canonical UTC timestamp from Unix-epoch nanoseconds.
    ///
    /// Keeping this conversion in the protocol crate lets effectful adapters
    /// project monotonic coordinator timestamps without constructing unchecked
    /// wire strings.
    pub fn from_unix_timestamp_nanos(value: i128) -> Result<Self, TimestampError> {
        let parsed =
            OffsetDateTime::from_unix_timestamp_nanos(value).map_err(|_| TimestampError)?;
        let canonical = parsed
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|_| TimestampError)?;
        Ok(Self(canonical))
    }

    /// Returns the canonical timestamp text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the represented instant as Unix epoch nanoseconds.
    ///
    /// This is useful for ordering independently parsed protocol timestamps.
    pub fn unix_timestamp_nanos(&self) -> Result<i128, TimestampError> {
        OffsetDateTime::parse(&self.0, &Rfc3339)
            .map(OffsetDateTime::unix_timestamp_nanos)
            .map_err(|_| TimestampError)
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Timestamp").field(&self.0).finish()
    }
}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.unix_timestamp_nanos(), other.unix_timestamp_nanos()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            // All constructors validate, so this fallback only preserves total
            // ordering if an invariant is violated by future internal code.
            (Err(_), Ok(_)) => Ordering::Less,
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Err(_)) => self.0.cmp(&other.0),
        }
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

/// Invalid RFC 3339 timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("timestamp must be valid RFC 3339")]
pub struct TimestampError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_offsets_to_utc() -> Result<(), TimestampError> {
        let value = Timestamp::parse("2026-07-20T10:00:00-07:00")?;
        assert_eq!(value.as_str(), "2026-07-20T17:00:00Z");
        Ok(())
    }

    #[test]
    fn unix_nanoseconds_round_trip_through_canonical_wire_form() -> Result<(), TimestampError> {
        let timestamp = Timestamp::from_unix_timestamp_nanos(1_721_433_600_123_456_789)?;
        assert_eq!(timestamp.unix_timestamp_nanos()?, 1_721_433_600_123_456_789);
        assert!(timestamp.as_str().ends_with('Z'));
        Ok(())
    }

    #[test]
    fn ordering_uses_instants_not_fractional_timestamp_text() -> Result<(), TimestampError> {
        let whole = Timestamp::parse("2026-07-20T00:00:00Z")?;
        let fractional = Timestamp::parse("2026-07-20T00:00:00.5Z")?;
        assert!(fractional > whole);
        Ok(())
    }
}
