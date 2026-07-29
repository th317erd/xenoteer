//! Canonical decimal-string adapters for precision-sensitive wire integers.

use schemars::{Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serializer, de};

const MAX_U64_DECIMAL_DIGITS: usize = 20;
const UINT64_STRING_FORMAT: &str = "uint64-string";

fn parse_canonical(value: &str) -> Result<u64, &'static str> {
    if value.is_empty()
        || value.len() > MAX_U64_DECIMAL_DIGITS
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("unsigned 64-bit integer must be a canonical decimal string");
    }
    value
        .parse()
        .map_err(|_| "unsigned 64-bit decimal string is out of range")
}

fn parse_nonzero(value: &str) -> Result<u64, &'static str> {
    let value = parse_canonical(value)?;
    if value == 0 {
        return Err("unsigned 64-bit decimal string must be non-zero");
    }
    Ok(value)
}

fn schema_with_pattern(pattern: &'static str) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "format": UINT64_STRING_FORMAT,
        "minLength": 1,
        "maxLength": MAX_U64_DECIMAL_DIGITS,
        "pattern": pattern
    })
}

pub(crate) fn schema(_: &mut SchemaGenerator) -> Schema {
    schema_with_pattern("^(0|[1-9][0-9]{0,19})$")
}

pub(crate) fn nonzero_schema(_: &mut SchemaGenerator) -> Schema {
    schema_with_pattern("^[1-9][0-9]{0,19}$")
}

pub(crate) fn optional_schema(_: &mut SchemaGenerator) -> Schema {
    let value = schema_with_pattern("^(0|[1-9][0-9]{0,19})$");
    schemars::json_schema!({
        "anyOf": [
            value,
            {"type": "null"}
        ]
    })
}

pub(crate) mod canonical {
    use super::*;

    pub(crate) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_canonical(&value).map_err(de::Error::custom)
    }
}

pub(crate) mod nonzero {
    use super::*;

    pub(crate) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_nonzero(&value).map_err(de::Error::custom)
    }
}

pub(crate) mod optional {
    use super::*;

    pub(crate) fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|value| parse_canonical(&value).map_err(de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    struct Fixture {
        #[serde(with = "super::canonical")]
        #[schemars(schema_with = "super::schema")]
        value: u64,
        #[serde(with = "super::optional")]
        #[schemars(schema_with = "super::optional_schema")]
        optional: Option<u64>,
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    struct NonzeroFixture {
        #[serde(with = "super::nonzero")]
        #[schemars(schema_with = "super::nonzero_schema")]
        value: u64,
    }

    #[test]
    fn canonical_decimal_strings_round_trip_without_precision_loss()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture {
            value: u64::MAX,
            optional: Some(9_007_199_254_740_993),
        };
        let encoded = serde_json::to_value(&fixture)?;
        assert_eq!(
            encoded,
            json!({
                "value": "18446744073709551615",
                "optional": "9007199254740993"
            })
        );
        assert_eq!(serde_json::from_value::<Fixture>(encoded)?, fixture);
        Ok(())
    }

    #[test]
    fn decoder_rejects_numbers_noncanonical_text_and_overflow() {
        for value in [
            json!(1),
            json!(""),
            json!("01"),
            json!("+1"),
            json!("-1"),
            json!(" 1"),
            json!("18446744073709551616"),
        ] {
            assert!(
                serde_json::from_value::<Fixture>(json!({
                    "value": value,
                    "optional": null
                }))
                .is_err()
            );
        }
        assert!(serde_json::from_value::<NonzeroFixture>(json!({"value": "0"})).is_err());
    }

    #[test]
    fn generated_schema_declares_the_custom_wire_format() -> Result<(), Box<dyn std::error::Error>>
    {
        let schema = serde_json::to_value(schemars::schema_for!(Fixture))?;
        assert_eq!(
            schema.pointer("/properties/value/type"),
            Some(&Value::from("string"))
        );
        assert_eq!(
            schema.pointer("/properties/value/format"),
            Some(&Value::from("uint64-string"))
        );
        assert_eq!(
            schema.pointer("/properties/optional/anyOf/0/format"),
            Some(&Value::from("uint64-string"))
        );
        Ok(())
    }
}
