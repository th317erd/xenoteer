//! UUID-backed public identifiers.

use core::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
            JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a cryptographically random version-4 identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an already validated UUID.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the UUID value.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

id_type!(DesktopId, "Identifies a desktop resource.");
id_type!(
    DesktopGeneration,
    "Identifies one X server/session lifetime."
);
id_type!(CommandId, "Deduplicates one client-authored command.");
id_type!(RequestId, "Correlates one transport request and response.");
id_type!(
    ControlLeaseId,
    "Identifies the current physical-input lease."
);
id_type!(ArtifactId, "Identifies a bounded server-side artifact.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_as_uuid_strings() -> Result<(), Box<dyn std::error::Error>> {
        let id = CommandId::new();
        let encoded = serde_json::to_string(&id)?;
        let decoded: CommandId = serde_json::from_str(&encoded)?;
        assert_eq!(decoded, id);
        Ok(())
    }
}
