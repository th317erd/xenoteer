//! Deterministic JSON Schema generation and golden-file checking.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

use crate::{
    AccessibilityEvent, ApplicationRef, ArtifactRef, CapabilityReport, ClientHello,
    ClipboardReadRequest, ClipboardReadResult, CommandEnvelope, CommandResult, ElementListPage,
    ElementListRequest, ElementQueryPage, ElementQueryRequest, ElementRef, ElementResolveRequest,
    ElementResolveResult, ElementSnapshotRequest, ElementSnapshotResult, ElementWaitRequest,
    ElementWaitResult, LeaseAcquireRequest, LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView,
    OneTimeViewerTicket, Problem, ProcessExitedEvent, ProcessRef, ProcessView, ScreenshotRequest,
    ScreenshotResult, ViewerSessionEvidence, ViewerTicketRequest, WebSocketClientMessage,
    WebSocketServerMessage, WindowListPage, WindowListRequest, WindowManagerCapabilities,
    WindowQueryPage, WindowQueryRequest, WindowResolveRequest, WindowResolveResult,
    WindowSnapshotRequest, WindowSnapshotResult, WindowWaitRequest, WindowWaitResult,
};

/// Checked-in schema filenames for protocol version one.
pub const SCHEMA_FILENAMES: [&str; 46] = [
    "capabilities.json",
    "command-envelope.json",
    "command-result.json",
    "problem.json",
    "lease-acquire-request.json",
    "lease-renew-request.json",
    "lease-release-request.json",
    "lease-state.json",
    "process-ref.json",
    "process-view.json",
    "process-exited-event.json",
    "websocket-client-message.json",
    "websocket-server-message.json",
    "websocket-client-hello.json",
    "artifact-ref.json",
    "screenshot-request.json",
    "screenshot-result.json",
    "clipboard-read-request.json",
    "clipboard-read-result.json",
    "window-list-request.json",
    "window-list-page.json",
    "window-snapshot-request.json",
    "window-snapshot-result.json",
    "window-query-request.json",
    "window-query-page.json",
    "window-resolve-request.json",
    "window-resolve-result.json",
    "window-wait-request.json",
    "window-wait-result.json",
    "window-manager-capabilities.json",
    "viewer-ticket-request.json",
    "viewer-ticket.json",
    "viewer-session-evidence.json",
    "application-ref.json",
    "element-ref.json",
    "element-list-request.json",
    "element-list-page.json",
    "element-query-request.json",
    "element-query-page.json",
    "element-snapshot-request.json",
    "element-snapshot-result.json",
    "element-wait-request.json",
    "element-wait-result.json",
    "element-resolve-request.json",
    "element-resolve-result.json",
    "accessibility-event.json",
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
        (
            SCHEMA_FILENAMES[4],
            serde_json::to_value(schemars::schema_for!(LeaseAcquireRequest))?,
        ),
        (
            SCHEMA_FILENAMES[5],
            serde_json::to_value(schemars::schema_for!(LeaseRenewRequest))?,
        ),
        (
            SCHEMA_FILENAMES[6],
            serde_json::to_value(schemars::schema_for!(LeaseReleaseRequest))?,
        ),
        (
            SCHEMA_FILENAMES[7],
            serde_json::to_value(schemars::schema_for!(LeaseStateView))?,
        ),
        (
            SCHEMA_FILENAMES[8],
            serde_json::to_value(schemars::schema_for!(ProcessRef))?,
        ),
        (
            SCHEMA_FILENAMES[9],
            serde_json::to_value(schemars::schema_for!(ProcessView))?,
        ),
        (
            SCHEMA_FILENAMES[10],
            serde_json::to_value(schemars::schema_for!(ProcessExitedEvent))?,
        ),
        (
            SCHEMA_FILENAMES[11],
            serde_json::to_value(schemars::schema_for!(WebSocketClientMessage))?,
        ),
        (
            SCHEMA_FILENAMES[12],
            serde_json::to_value(schemars::schema_for!(WebSocketServerMessage))?,
        ),
        (
            SCHEMA_FILENAMES[13],
            serde_json::to_value(schemars::schema_for!(ClientHello))?,
        ),
        (
            SCHEMA_FILENAMES[14],
            serde_json::to_value(schemars::schema_for!(ArtifactRef))?,
        ),
        (
            SCHEMA_FILENAMES[15],
            serde_json::to_value(schemars::schema_for!(ScreenshotRequest))?,
        ),
        (
            SCHEMA_FILENAMES[16],
            serde_json::to_value(schemars::schema_for!(ScreenshotResult))?,
        ),
        (
            SCHEMA_FILENAMES[17],
            serde_json::to_value(schemars::schema_for!(ClipboardReadRequest))?,
        ),
        (
            SCHEMA_FILENAMES[18],
            serde_json::to_value(schemars::schema_for!(ClipboardReadResult))?,
        ),
        (
            SCHEMA_FILENAMES[19],
            serde_json::to_value(schemars::schema_for!(WindowListRequest))?,
        ),
        (
            SCHEMA_FILENAMES[20],
            serde_json::to_value(schemars::schema_for!(WindowListPage))?,
        ),
        (
            SCHEMA_FILENAMES[21],
            serde_json::to_value(schemars::schema_for!(WindowSnapshotRequest))?,
        ),
        (
            SCHEMA_FILENAMES[22],
            serde_json::to_value(schemars::schema_for!(WindowSnapshotResult))?,
        ),
        (
            SCHEMA_FILENAMES[23],
            serde_json::to_value(schemars::schema_for!(WindowQueryRequest))?,
        ),
        (
            SCHEMA_FILENAMES[24],
            serde_json::to_value(schemars::schema_for!(WindowQueryPage))?,
        ),
        (
            SCHEMA_FILENAMES[25],
            serde_json::to_value(schemars::schema_for!(WindowResolveRequest))?,
        ),
        (
            SCHEMA_FILENAMES[26],
            serde_json::to_value(schemars::schema_for!(WindowResolveResult))?,
        ),
        (
            SCHEMA_FILENAMES[27],
            serde_json::to_value(schemars::schema_for!(WindowWaitRequest))?,
        ),
        (
            SCHEMA_FILENAMES[28],
            serde_json::to_value(schemars::schema_for!(WindowWaitResult))?,
        ),
        (
            SCHEMA_FILENAMES[29],
            serde_json::to_value(schemars::schema_for!(WindowManagerCapabilities))?,
        ),
        (
            SCHEMA_FILENAMES[30],
            serde_json::to_value(schemars::schema_for!(ViewerTicketRequest))?,
        ),
        (
            SCHEMA_FILENAMES[31],
            serde_json::to_value(schemars::schema_for!(OneTimeViewerTicket))?,
        ),
        (
            SCHEMA_FILENAMES[32],
            serde_json::to_value(schemars::schema_for!(ViewerSessionEvidence))?,
        ),
        (
            SCHEMA_FILENAMES[33],
            serde_json::to_value(schemars::schema_for!(ApplicationRef))?,
        ),
        (
            SCHEMA_FILENAMES[34],
            serde_json::to_value(schemars::schema_for!(ElementRef))?,
        ),
        (
            SCHEMA_FILENAMES[35],
            serde_json::to_value(schemars::schema_for!(ElementListRequest))?,
        ),
        (
            SCHEMA_FILENAMES[36],
            serde_json::to_value(schemars::schema_for!(ElementListPage))?,
        ),
        (
            SCHEMA_FILENAMES[37],
            serde_json::to_value(schemars::schema_for!(ElementQueryRequest))?,
        ),
        (
            SCHEMA_FILENAMES[38],
            serde_json::to_value(schemars::schema_for!(ElementQueryPage))?,
        ),
        (
            SCHEMA_FILENAMES[39],
            serde_json::to_value(schemars::schema_for!(ElementSnapshotRequest))?,
        ),
        (
            SCHEMA_FILENAMES[40],
            serde_json::to_value(schemars::schema_for!(ElementSnapshotResult))?,
        ),
        (
            SCHEMA_FILENAMES[41],
            serde_json::to_value(schemars::schema_for!(ElementWaitRequest))?,
        ),
        (
            SCHEMA_FILENAMES[42],
            serde_json::to_value(schemars::schema_for!(ElementWaitResult))?,
        ),
        (
            SCHEMA_FILENAMES[43],
            serde_json::to_value(schemars::schema_for!(ElementResolveRequest))?,
        ),
        (
            SCHEMA_FILENAMES[44],
            serde_json::to_value(schemars::schema_for!(ElementResolveResult))?,
        ),
        (
            SCHEMA_FILENAMES[45],
            serde_json::to_value(schemars::schema_for!(AccessibilityEvent))?,
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
    fn request_schema_roots_reject_additive_fields() -> Result<(), Box<dyn std::error::Error>> {
        let object_root_names = [
            "command-envelope.json",
            "lease-acquire-request.json",
            "lease-renew-request.json",
            "lease-release-request.json",
            "websocket-client-hello.json",
            "screenshot-request.json",
            "clipboard-read-request.json",
            "window-list-request.json",
            "window-snapshot-request.json",
            "window-query-request.json",
            "window-resolve-request.json",
            "window-wait-request.json",
            "viewer-ticket-request.json",
            "element-list-request.json",
            "element-query-request.json",
            "element-resolve-request.json",
            "element-snapshot-request.json",
            "element-wait-request.json",
        ];
        for name in object_root_names {
            let schema = generated_schema(name)?;
            assert_closed_object(&schema, name);
            assert_all_object_schemas_have_policy(&schema, false, name, "#");
        }

        let schema = generated_schema("websocket-client-message.json")?;
        assert_object_variants_have_policy(&schema, "/oneOf", false, "websocket client message")?;
        assert_all_object_schemas_have_policy(&schema, false, "websocket-client-message.json", "#");
        Ok(())
    }

    #[test]
    fn response_schema_roots_tolerate_additive_fields() -> Result<(), Box<dyn std::error::Error>> {
        for name in [
            "capabilities.json",
            "command-result.json",
            "problem.json",
            "lease-state.json",
            "process-ref.json",
            "process-view.json",
            "process-exited-event.json",
            "artifact-ref.json",
            "screenshot-result.json",
            "clipboard-read-result.json",
            "window-list-page.json",
            "window-snapshot-result.json",
            "window-query-page.json",
            "window-resolve-result.json",
            "window-wait-result.json",
            "window-manager-capabilities.json",
            "viewer-ticket.json",
            "viewer-session-evidence.json",
            "application-ref.json",
            "element-ref.json",
            "element-list-page.json",
            "element-query-page.json",
            "element-resolve-result.json",
            "element-snapshot-result.json",
            "element-wait-result.json",
            "accessibility-event.json",
        ] {
            let schema = generated_schema(name)?;
            assert_open_object(&schema, name);
            assert_all_object_schemas_have_policy(&schema, true, name, "#");
        }

        let schema = generated_schema("websocket-server-message.json")?;
        assert_object_variants_have_policy(&schema, "/oneOf", true, "websocket server message")?;
        assert_all_object_schemas_have_policy(&schema, true, "websocket-server-message.json", "#");
        Ok(())
    }

    #[test]
    fn shared_nested_schema_objects_follow_message_direction()
    -> Result<(), Box<dyn std::error::Error>> {
        let command = generated_schema("command-envelope.json")?;
        for definition in [
            "ArtifactRef",
            "KeyboardSequenceStep",
            "Point",
            "ProcessRef",
            "ProtocolVersion",
            "SecretInlineBinary",
            "WindowGeometryRequest",
            "WindowRef",
        ] {
            assert_closed_object(
                required_pointer(&command, &format!("/$defs/{definition}"))?,
                &format!("command-envelope {definition}"),
            );
        }
        for definition in [
            "KeyboardKeyIdentifier",
            "PointerClickTarget",
            "PointerDragTarget",
        ] {
            assert_object_variants_have_policy(
                &command,
                &format!("/$defs/{definition}/oneOf"),
                false,
                &format!("command-envelope {definition}"),
            )?;
        }

        let screenshot_request = generated_schema("screenshot-request.json")?;
        assert_closed_object(
            required_pointer(&screenshot_request, "/$defs/Rect")?,
            "screenshot-request Rect",
        );
        assert_object_variants_have_policy(
            &screenshot_request,
            "/$defs/ScreenshotTarget/oneOf",
            false,
            "screenshot-request ScreenshotTarget",
        )?;
        assert_closed_object(
            required_pointer(&screenshot_request, "/$defs/WindowRef")?,
            "screenshot-request WindowRef",
        );

        let window_wait_request = generated_schema("window-wait-request.json")?;
        for definition in ["Point", "ProcessRef", "Rect", "WindowRect", "WindowRef"] {
            assert_closed_object(
                required_pointer(&window_wait_request, &format!("/$defs/{definition}"))?,
                &format!("window-wait-request {definition}"),
            );
        }

        let screenshot_result = generated_schema("screenshot-result.json")?;
        for definition in [
            "ArtifactRef",
            "CursorCaptureEvidence",
            "RawBgraMetadata",
            "Size",
            "WindowRef",
        ] {
            assert_open_object(
                required_pointer(&screenshot_result, &format!("/$defs/{definition}"))?,
                &format!("screenshot-result {definition}"),
            );
        }
        for definition in ["ScreenshotDelivery", "ScreenshotTarget"] {
            assert_object_variants_have_policy(
                &screenshot_result,
                &format!("/$defs/{definition}/oneOf"),
                true,
                &format!("screenshot-result {definition}"),
            )?;
        }

        let window_page = generated_schema("window-list-page.json")?;
        for definition in [
            "ProcessRef",
            "Rect",
            "WindowClass",
            "WindowFrameExtents",
            "WindowGeometry",
            "WindowMetadata",
            "WindowObservedState",
            "WindowProcessCorrelation",
            "WindowRect",
            "WindowRef",
            "WindowSnapshot",
            "WindowSnapshotEntry",
            "WindowText",
        ] {
            assert_open_object(
                required_pointer(&window_page, &format!("/$defs/{definition}"))?,
                &format!("window-list-page {definition}"),
            );
        }
        let entry = required_pointer(&window_page, "/$defs/WindowSnapshotEntry")?;
        let required = entry
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| std::io::Error::other("WindowSnapshotEntry required fields missing"))?;
        assert!(required.contains(&Value::from("snapshot")));
        assert!(required.contains(&Value::from("reference_token")));
        Ok(())
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

    fn generated_schema(name: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let (_, encoded) = generated_schemas()?
            .into_iter()
            .find(|(candidate, _)| *candidate == name)
            .ok_or_else(|| std::io::Error::other(format!("schema was not generated: {name}")))?;
        Ok(serde_json::from_str(&encoded)?)
    }

    fn required_pointer<'a>(schema: &'a Value, pointer: &str) -> Result<&'a Value, std::io::Error> {
        schema
            .pointer(pointer)
            .ok_or_else(|| std::io::Error::other(format!("schema pointer is missing: {pointer}")))
    }

    fn assert_closed_object(schema: &Value, label: &str) {
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{label} must reject additive fields"
        );
    }

    fn assert_open_object(schema: &Value, label: &str) {
        assert_ne!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{label} must tolerate additive fields"
        );
    }

    fn assert_object_variants_have_policy(
        schema: &Value,
        pointer: &str,
        open: bool,
        label: &str,
    ) -> Result<(), std::io::Error> {
        let variants = required_pointer(schema, pointer)?
            .as_array()
            .ok_or_else(|| std::io::Error::other(format!("{label} variants must be an array")))?;
        assert!(!variants.is_empty(), "{label} must define object variants");
        for (index, variant) in variants.iter().enumerate() {
            if open {
                assert_open_object(variant, &format!("{label} variant {index}"));
            } else {
                assert_closed_object(variant, &format!("{label} variant {index}"));
            }
        }
        Ok(())
    }

    fn assert_all_object_schemas_have_policy(
        value: &Value,
        open: bool,
        label: &str,
        pointer: &str,
    ) {
        if value.get("type") == Some(&Value::from("object")) {
            if open {
                assert_open_object(value, &format!("{label} at {pointer}"));
            } else {
                assert_closed_object(value, &format!("{label} at {pointer}"));
            }
        }

        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    assert_all_object_schemas_have_policy(
                        child,
                        open,
                        label,
                        &format!("{pointer}/{key}"),
                    );
                }
            }
            Value::Array(array) => {
                for (index, child) in array.iter().enumerate() {
                    assert_all_object_schemas_have_policy(
                        child,
                        open,
                        label,
                        &format!("{pointer}/{index}"),
                    );
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
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
