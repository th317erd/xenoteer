//! Deterministic JSON Schema generation and golden-file checking.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

use crate::{CapabilityReport, CommandEnvelope, CommandResult, Problem};

/// Checked-in schema filenames for protocol version one.
pub const SCHEMA_FILENAMES: [&str; 4] = [
    "capabilities.json",
    "command-envelope.json",
    "command-result.json",
    "problem.json",
];

/// Returns the repository's checked-in version-one schema directory.
#[must_use]
pub fn checked_in_schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1")
}

/// Produces all version-one schemas with recursively sorted object keys.
pub fn generated_schemas() -> Result<Vec<(&'static str, String)>, SchemaError> {
    let schemas = [
        (
            SCHEMA_FILENAMES[0],
            serde_json::to_value(schemars::schema_for!(CapabilityReport))?,
        ),
        (
            SCHEMA_FILENAMES[1],
            serde_json::to_value(schemars::schema_for!(CommandEnvelope))?,
        ),
        (
            SCHEMA_FILENAMES[2],
            serde_json::to_value(schemars::schema_for!(CommandResult))?,
        ),
        (
            SCHEMA_FILENAMES[3],
            serde_json::to_value(schemars::schema_for!(Problem))?,
        ),
    ];

    schemas
        .into_iter()
        .map(|(name, mut value)| {
            sort_json(&mut value);
            let mut encoded = serde_json::to_string_pretty(&value)?;
            encoded.push('\n');
            Ok((name, encoded))
        })
        .collect()
}

/// Writes generated schemas, or checks that existing files match exactly.
pub fn write_or_check(directory: &Path, check: bool) -> Result<(), SchemaError> {
    let schemas = generated_schemas()?;
    if check {
        for (name, expected) in schemas {
            let path = directory.join(name);
            let actual = fs::read_to_string(&path).map_err(|source| SchemaError::Read {
                path: path.clone(),
                source,
            })?;
            if actual != expected {
                return Err(SchemaError::Drift(path));
            }
        }
        for entry in fs::read_dir(directory).map_err(|source| SchemaError::Read {
            path: directory.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| SchemaError::Read {
                path: directory.to_path_buf(),
                source,
            })?;
            if !SCHEMA_FILENAMES
                .iter()
                .any(|expected| entry.file_name() == OsStr::new(expected))
            {
                return Err(SchemaError::UnexpectedFile(entry.path()));
            }
        }
        return Ok(());
    }

    fs::create_dir_all(directory).map_err(|source| SchemaError::Write {
        path: directory.to_path_buf(),
        source,
    })?;
    for (name, contents) in schemas {
        let path = directory.join(name);
        fs::write(&path, contents).map_err(|source| SchemaError::Write { path, source })?;
    }
    Ok(())
}

fn sort_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                sort_json(child);
            }
            object.sort_keys();
        }
        Value::Array(array) => array.iter_mut().for_each(sort_json),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Schema generation/check failure.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// Schema serialization failed.
    #[error("failed to serialize generated schema: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A checked-in schema could not be read.
    #[error("failed to read schema {path}: {source}")]
    Read {
        /// Schema path.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// A generated schema could not be written.
    #[error("failed to write schema {path}: {source}")]
    Write {
        /// Schema path.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// A checked-in schema differs from current generation.
    #[error("generated schema differs from checked-in file {0}")]
    Drift(PathBuf),
    /// The schema directory contains a stale or unrecognized entry.
    #[error("schema directory contains unexpected entry {0}")]
    UnexpectedFile(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rect, Size};

    #[test]
    fn checked_in_schemas_are_current() -> Result<(), SchemaError> {
        write_or_check(&checked_in_schema_dir(), true)
    }

    #[test]
    fn pointer_duration_schema_has_runtime_maximum() -> Result<(), Box<dyn std::error::Error>> {
        let (_, encoded) = generated_schemas()?
            .into_iter()
            .find(|(name, _)| *name == "command-envelope.json")
            .ok_or_else(|| std::io::Error::other("command envelope schema was not generated"))?;
        let schema: Value = serde_json::from_str(&encoded)?;
        let duration = schema
            .pointer("/$defs/Command/oneOf/1/properties/duration_ms")
            .ok_or_else(|| std::io::Error::other("pointer duration schema is missing"))?;
        let integer_branch = non_null_integer_branch(duration)
            .ok_or_else(|| std::io::Error::other("pointer duration integer branch is missing"))?;
        assert_eq!(
            integer_branch.get("maximum").and_then(Value::as_u64),
            Some(10_000)
        );
        Ok(())
    }

    #[test]
    fn problem_schema_has_runtime_public_output_bounds() -> Result<(), Box<dyn std::error::Error>> {
        let (_, encoded) = generated_schemas()?
            .into_iter()
            .find(|(name, _)| *name == "problem.json")
            .ok_or_else(|| std::io::Error::other("problem schema was not generated"))?;
        let schema: Value = serde_json::from_str(&encoded)?;
        assert_eq!(
            schema.pointer("/properties/status/minimum"),
            Some(&Value::from(400))
        );
        assert_eq!(
            schema.pointer("/properties/status/maximum"),
            Some(&Value::from(599))
        );
        assert_eq!(
            schema.pointer("/properties/title/maxLength"),
            Some(&Value::from(128))
        );
        assert_eq!(
            schema.pointer("/properties/detail/maxLength"),
            Some(&Value::from(1_024))
        );
        assert_eq!(
            schema.pointer("/properties/details/maxProperties"),
            Some(&Value::from(16))
        );
        assert_eq!(
            schema.pointer("/properties/details/propertyNames/maxLength"),
            Some(&Value::from(64))
        );
        Ok(())
    }

    #[test]
    fn capability_schema_exposes_identifier_and_optional_text_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_, encoded) = generated_schemas()?
            .into_iter()
            .find(|(name, _)| *name == "capabilities.json")
            .ok_or_else(|| std::io::Error::other("capability schema was not generated"))?;
        let schema: Value = serde_json::from_str(&encoded)?;
        assert_eq!(
            schema.pointer("/$defs/CapabilityId/minLength"),
            Some(&Value::from(1))
        );
        assert_eq!(
            schema.pointer("/$defs/CapabilityId/maxLength"),
            Some(&Value::from(128))
        );
        assert!(schema.pointer("/$defs/CapabilityId/pattern").is_some());
        assert_eq!(
            schema.pointer("/$defs/Capability/properties/reason_code/minLength"),
            Some(&Value::from(1))
        );
        assert!(
            schema
                .pointer("/$defs/Capability/properties/reason_code/pattern")
                .is_some()
        );
        assert_eq!(
            schema.pointer("/$defs/Capability/properties/backend_version/minLength"),
            Some(&Value::from(1))
        );
        assert_eq!(
            schema.pointer("/properties/capabilities/maxItems"),
            Some(&Value::from(256))
        );
        Ok(())
    }

    #[test]
    fn geometry_schemas_require_non_empty_extents() -> Result<(), Box<dyn std::error::Error>> {
        for schema in [
            serde_json::to_value(schemars::schema_for!(Size))?,
            serde_json::to_value(schemars::schema_for!(Rect))?,
        ] {
            assert_eq!(
                schema.pointer("/properties/width/minimum"),
                Some(&Value::from(1))
            );
            assert_eq!(
                schema.pointer("/properties/height/minimum"),
                Some(&Value::from(1))
            );
        }
        Ok(())
    }

    #[test]
    fn result_schema_exposes_timestamp_and_warning_constraints()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_, encoded) = generated_schemas()?
            .into_iter()
            .find(|(name, _)| *name == "command-result.json")
            .ok_or_else(|| std::io::Error::other("command-result schema was not generated"))?;
        let schema: Value = serde_json::from_str(&encoded)?;
        assert_eq!(
            schema.pointer("/$defs/Timestamp/format"),
            Some(&Value::from("date-time"))
        );
        assert_eq!(
            schema.pointer("/$defs/Warning/properties/code/minLength"),
            Some(&Value::from(1))
        );
        assert!(
            schema
                .pointer("/$defs/Warning/properties/code/pattern")
                .is_some()
        );
        assert_eq!(
            schema.pointer("/$defs/Warning/properties/message/minLength"),
            Some(&Value::from(1))
        );
        Ok(())
    }

    #[test]
    fn check_rejects_stale_extra_schema_file() -> Result<(), Box<dyn std::error::Error>> {
        let directory =
            std::env::temp_dir().join(format!("xenoteer-schema-test-{}", uuid::Uuid::new_v4()));
        let guard = TestDirectory::new(directory.clone());
        write_or_check(&directory, false)?;
        fs::write(directory.join("stale.json"), "{}\n")?;
        let result = write_or_check(&directory, true);
        assert!(matches!(result, Err(SchemaError::UnexpectedFile(_))));
        drop(guard);
        Ok(())
    }

    fn non_null_integer_branch(schema: &Value) -> Option<&Value> {
        let has_integer_type = match schema.get("type") {
            Some(Value::String(value)) => value == "integer",
            Some(Value::Array(values)) => values.iter().any(|value| value == "integer"),
            Some(_) | None => false,
        };
        if has_integer_type {
            return Some(schema);
        }
        schema
            .get("anyOf")?
            .as_array()?
            .iter()
            .find(|branch| non_null_integer_branch(branch).is_some())
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(path: PathBuf) -> Self {
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
