//! Internal language-neutral conformance adapter implementation.

#![allow(missing_docs)]

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    time::{sleep, timeout},
};
use xenoteer_protocol::{
    ArtifactContentType, Command, CommandEnvelope, CommandId, CommandResult, ControlLeaseId,
    DesktopGeneration, DesktopId, ElementRef, ElementSnapshotExpansion, ElementSnapshotRequest,
    EventResumeRequest, EventTopic, LeaseAvailability, LeaseStateView, OneTimeViewerTicket,
    Problem, ProtocolVersion, RequestId, SequencedEvent, StatusResponse, Timestamp, VersionError,
    VersionRange, WebSocketServerMessage, WindowRef, WindowReferenceToken,
};

use crate::{
    Client, ControlLease, Desktop, ElementHandle, EventStreamCloseReason, EventStreamItem,
    EventStreamResyncReason, SdkError, WindowHandle,
    events::{EventConfiguration, handle_server_text},
};

const ADAPTER_PROTOCOL: u32 = 1;
const CORPUS_ID: &str = "xenoteer-conformance-v1";
const CORPUS_SHA256: &str = "6cc98e72e1de6591cce2d0661f4fc3ea508535d310a40746aa3ad8bd1e61e7fc";
const TEST_TOKEN: &str = "conformance-test-token-0123456789abcdef";

#[derive(Deserialize)]
struct AdapterRequest {
    adapter_protocol: u32,
    corpus: String,
    corpus_sha256: String,
    protocol: Value,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    operation: String,
    input: Value,
    expect: Value,
}

#[derive(Serialize)]
struct AdapterResponse {
    adapter_protocol: u32,
    results: Vec<CaseResult>,
}

#[derive(Serialize)]
struct CaseResult {
    detail: String,
    id: String,
    status: &'static str,
}

/// Reads one runner request and writes one runner response.
pub async fn run_adapter(
    input: impl Read,
    mut output: impl Write,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut encoded = Vec::new();
    input.take(8 * 1024 * 1024).read_to_end(&mut encoded)?;
    let request: AdapterRequest = serde_json::from_slice(&encoded)?;
    if request.adapter_protocol != ADAPTER_PROTOCOL
        || request.corpus != CORPUS_ID
        || request.corpus_sha256 != CORPUS_SHA256
        || request.protocol != json!({"major": 1, "min_minor": 0, "max_minor": 0})
        || request.cases.is_empty()
    {
        return Err("invalid conformance adapter request".into());
    }
    runtime_lifecycle_self_check().await?;
    let mut results = Vec::with_capacity(request.cases.len());
    for case in request.cases {
        let result = match evaluate(&case.operation, &case.input).await {
            Ok(actual) if expectation_matches(&actual, &case.expect) => CaseResult {
                detail: String::new(),
                id: case.id,
                status: "passed",
            },
            Ok(actual) => CaseResult {
                detail: format!(
                    "observed {} but expected {}",
                    compact(&actual),
                    compact(&case.expect)
                ),
                id: case.id,
                status: "failed",
            },
            Err(detail) => CaseResult {
                detail,
                id: case.id,
                status: "failed",
            },
        };
        results.push(result);
    }
    serde_json::to_writer(
        &mut output,
        &AdapterResponse {
            adapter_protocol: ADAPTER_PROTOCOL,
            results,
        },
    )?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

async fn evaluate(operation: &str, input: &Value) -> Result<Value, String> {
    match operation {
        "decode_uint64_string" => decode_uint64(input),
        "negotiate_protocol_range" => negotiate(input),
        "admit_request_version" => admit_request_version(input),
        "decode_request" => decode_request(input),
        "decode_response" => decode_response(input),
        "decode_event" => decode_event(input),
        "redaction" => redaction(input).await,
        "reference_lifecycle" => reference_lifecycle(input).await,
        "classify_terminal_effect" => classify_terminal_effect(input),
        "event_continuity" => event_continuity(input),
        "command_reconnect" => command_reconnect(input).await,
        other => Err(format!("unsupported conformance operation {other}")),
    }
}

fn expectation_matches(actual: &Value, expected: &Value) -> bool {
    let (Some(actual), Some(expected)) = (actual.as_object(), expected.as_object()) else {
        return actual == expected;
    };
    expected.iter().all(|(key, expected_value)| {
        let Some(actual_value) = actual.get(key) else {
            return false;
        };
        if key == "preserve" {
            string_set(actual_value) == string_set(expected_value)
        } else {
            actual_value == expected_value
        }
    })
}

fn string_set(value: &Value) -> Option<BTreeSet<&str>> {
    value
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<BTreeSet<_>>>()
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unencodable>".to_owned())
}

fn required_object<'a>(value: &'a Value, field: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{field} must be an object"))
}

fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be text"))
}

fn parse_wire_value<T: DeserializeOwned>(value: &Value, description: &str) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|_| format!("{description} is invalid"))
}

fn required_wire<T: DeserializeOwned>(value: &Value, field: &str) -> Result<T, String> {
    parse_wire_value(
        value
            .get(field)
            .ok_or_else(|| format!("{field} is required"))?,
        field,
    )
}

fn optional_wire<T: DeserializeOwned>(value: &Value, field: &str) -> Result<Option<T>, String> {
    value
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| parse_wire_value(value, field))
        .transpose()
}

fn parse_wire_u64(value: &Value, description: &str) -> Result<u64, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{description} must be a decimal string"))?;
    let parsed = text
        .parse::<u64>()
        .map_err(|_| format!("{description} is invalid"))?;
    if parsed.to_string() != text {
        return Err(format!("{description} is not canonical"));
    }
    Ok(parsed)
}

fn enum_text(value: impl Serialize) -> Result<String, String> {
    serde_json::to_value(value)
        .map_err(|_| "enum serialization failed".to_owned())?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "enum did not serialize as text".to_owned())
}

fn close_reason_text(reason: &EventStreamCloseReason) -> String {
    match reason {
        EventStreamCloseReason::QueueOverflow => "queue_overflow".to_owned(),
        EventStreamCloseReason::ResyncRequired => "resync_required".to_owned(),
        EventStreamCloseReason::ServerError(code) => {
            format!(
                "server_error:{}",
                enum_text(code).unwrap_or_else(|_| "unknown".to_owned())
            )
        }
        EventStreamCloseReason::ServerDraining => "server_draining".to_owned(),
        EventStreamCloseReason::PeerClosed(code) => format!("peer_closed:{code}"),
        EventStreamCloseReason::ProtocolViolation => "protocol_violation".to_owned(),
        EventStreamCloseReason::InvalidMessage { .. } => "invalid_message".to_owned(),
        EventStreamCloseReason::ClientClosed => "client_closed".to_owned(),
    }
}

fn required_u16(object: &Map<String, Value>, field: &str) -> Result<u16, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| format!("{field} must be an unsigned 16-bit integer"))
}

fn decode_uint64(input: &Value) -> Result<Value, String> {
    let allow_zero = input
        .get("allow_zero")
        .and_then(Value::as_bool)
        .ok_or_else(|| "allow_zero must be a boolean".to_owned())?;
    let wire = input
        .get("wire")
        .cloned()
        .ok_or_else(|| "wire is required".to_owned())?;
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let decoded = if allow_zero {
        serde_json::from_value::<EventResumeRequest>(json!({
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "event_sequence": wire
        }))
        .map(|value| value.event_sequence)
    } else {
        serde_json::from_value::<SequencedEvent>(json!({
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "sequence": wire,
            "topic": "conformance.counter",
            "payload": {}
        }))
        .map(|value| value.sequence)
    };
    Ok(match decoded {
        Ok(value) => json!({"outcome": "accepted", "decimal": value.to_string()}),
        Err(_) => json!({"outcome": "rejected", "code": "invalid_uint64_string"}),
    })
}

fn parse_range(value: &Value) -> Result<VersionRange, VersionError> {
    let Some(object) = value.as_object() else {
        return Err(VersionError::ReversedMinorRange);
    };
    let major = object
        .get("major")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or(VersionError::ReversedMinorRange)?;
    let minimum = object
        .get("min_minor")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or(VersionError::ReversedMinorRange)?;
    let maximum = object
        .get("max_minor")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or(VersionError::ReversedMinorRange)?;
    VersionRange::new(major, minimum, maximum)
}

fn negotiate(input: &Value) -> Result<Value, String> {
    let result = parse_range(
        input
            .get("client")
            .ok_or_else(|| "client range is required".to_owned())?,
    )
    .and_then(|client| {
        parse_range(
            input
                .get("server")
                .ok_or(VersionError::ReversedMinorRange)?,
        )
        .and_then(|server| client.negotiate(server))
    });
    Ok(match result {
        Ok(selected) => json!({
            "outcome": "accepted",
            "selected": {"major": selected.major(), "minor": selected.minor()}
        }),
        Err(error) => json!({
            "outcome": "rejected",
            "code": match error {
                VersionError::ReversedMinorRange => "reversed_minor_range",
                VersionError::UnsupportedMajor => "unsupported_major",
                VersionError::NoSharedMinor => "no_shared_minor",
            }
        }),
    })
}

fn admit_request_version(input: &Value) -> Result<Value, String> {
    let negotiated = required_object(input, "negotiated")?;
    let request = required_object(input, "request")?;
    let negotiated = ProtocolVersion::new(
        required_u16(negotiated, "major")?,
        required_u16(negotiated, "minor")?,
    );
    let request = ProtocolVersion::new(
        required_u16(request, "major")?,
        required_u16(request, "minor")?,
    );
    Ok(if VersionRange::exact(negotiated).contains(request) {
        json!({"outcome": "accepted"})
    } else {
        json!({"outcome": "rejected", "code": "unsupported_version"})
    })
}

fn decode_request(input: &Value) -> Result<Value, String> {
    let wire = input
        .get("wire")
        .cloned()
        .ok_or_else(|| "wire is required".to_owned())?;
    let accepted = serde_json::from_value::<CommandEnvelope>(wire)
        .and_then(|value| {
            value
                .validate()
                .map(|()| value)
                .map_err(serde::de::Error::custom)
        })
        .is_ok();
    Ok(if accepted {
        json!({"outcome": "accepted", "connection": "usable", "preserve": []})
    } else {
        json!({
            "outcome": "rejected",
            "code": "invalid_request",
            "connection": "usable",
            "preserve": []
        })
    })
}

fn decode_response(input: &Value) -> Result<Value, String> {
    let wire = input
        .get("wire")
        .cloned()
        .ok_or_else(|| "wire is required".to_owned())?;
    if wire.get("type").is_none() {
        let accepted = serde_json::from_value::<StatusResponse>(wire.clone())
            .and_then(|mut value| {
                value
                    .validate()
                    .map(|()| value)
                    .map_err(serde::de::Error::custom)
            })
            .is_ok();
        if !accepted {
            return Ok(json!({
                "outcome": "operation_error",
                "code": "invalid_response",
                "connection": "usable",
                "preserve": []
            }));
        }
        let mut preserve = Vec::new();
        let known_top = [
            "server_version",
            "protocol_min",
            "protocol_max",
            "server_time",
            "desktop",
            "capabilities",
        ];
        if let Some(object) = wire.as_object() {
            for key in object
                .keys()
                .filter(|key| !known_top.contains(&key.as_str()))
            {
                preserve.push(format!("/{key}"));
            }
        }
        for (field, known) in [
            ("protocol_min", &["major", "minor"][..]),
            ("protocol_max", &["major", "minor"][..]),
            ("desktop", &["id", "generation", "state", "reason_code"][..]),
        ] {
            if let Some(object) = wire.get(field).and_then(Value::as_object) {
                for key in object.keys().filter(|key| !known.contains(&key.as_str())) {
                    preserve.push(format!("/{field}/{key}"));
                }
            }
        }
        preserve.sort();
        return Ok(json!({
            "outcome": "accepted",
            "known_type": "status",
            "preserve": preserve
        }));
    }
    match serde_json::from_value::<WebSocketServerMessage>(wire.clone()) {
        Ok(_) => Ok(json!({"outcome": "accepted", "connection": "usable"})),
        Err(_) if wire.get("type").and_then(Value::as_str) == Some("command.result") => {
            let preserve = if wire.pointer("/result/outcome/type").is_some() {
                vec!["/result/outcome"]
            } else {
                Vec::new()
            };
            Ok(json!({
                "outcome": "operation_error",
                "code": "unsupported_response_variant",
                "connection": "usable",
                "preserve": preserve
            }))
        }
        Err(_) => Ok(json!({
            "outcome": "unknown_message",
            "connection": "usable",
            "preserve": [""]
        })),
    }
}

fn decode_event(input: &Value) -> Result<Value, String> {
    let wire = input
        .get("wire")
        .cloned()
        .ok_or_else(|| "wire is required".to_owned())?;
    let message: WebSocketServerMessage =
        serde_json::from_value(wire.clone()).map_err(|_| "event wire did not decode".to_owned())?;
    let WebSocketServerMessage::Event { event, .. } = message else {
        return Err("event operation received a non-event message".to_owned());
    };
    event
        .validate()
        .map_err(|_| "event failed protocol validation".to_owned())?;
    let unknown = event.topic.as_str().starts_with("future.");
    Ok(if unknown {
        json!({
            "outcome": "unknown_event",
            "preserve": ["/event/payload", "/event/sequence", "/event/topic"]
        })
    } else {
        json!({"outcome": "known_event", "preserve": ["/event/sequence"]})
    })
}

async fn redaction(input: &Value) -> Result<Value, String> {
    let kind = required_text(input, "kind")?;
    if !matches!(
        kind,
        "artifact" | "bearer" | "clipboard" | "command" | "viewer"
    ) {
        return Err("unsupported redaction kind".to_owned());
    }
    let secret = required_text(input, "secret")?;
    let base_url = required_text(input, "base_url")?;
    let raw = required_object(input, "raw")?;
    enum ParsedRedactionProbe {
        Artifact {
            content_type: ArtifactContentType,
            body: Bytes,
        },
        Status,
        Command(Box<Command>),
        Viewer(OneTimeViewerTicket),
    }
    let parsed_probe = match kind {
        "artifact" => {
            let content_type: ArtifactContentType = parse_wire_value(
                raw.get("content_type")
                    .ok_or_else(|| "content_type is required".to_owned())?,
                "artifact content type",
            )?;
            let body = raw
                .get("bytes_utf8")
                .and_then(Value::as_str)
                .ok_or_else(|| "bytes_utf8 must be text".to_owned())?;
            ParsedRedactionProbe::Artifact {
                content_type,
                body: Bytes::copy_from_slice(body.as_bytes()),
            }
        }
        "bearer" => ParsedRedactionProbe::Status,
        "clipboard" | "command" => {
            let command: Command = parse_wire_value(
                raw.get("command")
                    .ok_or_else(|| "command is required".to_owned())?,
                "command",
            )?;
            command
                .validate()
                .map_err(|_| "command failed validation".to_owned())?;
            ParsedRedactionProbe::Command(Box::new(command))
        }
        "viewer" => {
            let ticket: OneTimeViewerTicket = parse_wire_value(
                raw.get("ticket")
                    .ok_or_else(|| "ticket is required".to_owned())?,
                "viewer ticket",
            )?;
            ticket
                .validate()
                .map_err(|_| "viewer ticket failed validation".to_owned())?;
            ParsedRedactionProbe::Viewer(ticket)
        }
        _ => unreachable!("redaction kind was validated above"),
    };
    let configured_client = Client::new(base_url, secret.as_bytes())
        .map_err(|error| format!("client construction failed: {error}"))?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("redaction listener failed: {error}"))?;
    let fault_base = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|error| format!("redaction listener address failed: {error}"))?
    );
    let client = Client::new(fault_base, secret.as_bytes())
        .map_err(|error| format!("fault client construction failed: {error}"))?;

    let mut debug_surfaces = vec![format!("{configured_client:?}"), format!("{client:?}")];
    enum RedactionProbe {
        Artifact {
            content_type: ArtifactContentType,
            body: Bytes,
        },
        Status,
        Submission(Box<crate::CommandSubmission>),
    }
    let probe = match parsed_probe {
        ParsedRedactionProbe::Artifact { content_type, body } => {
            debug_surfaces.push(format!("{content_type:?}"));
            RedactionProbe::Artifact { content_type, body }
        }
        ParsedRedactionProbe::Status => RedactionProbe::Status,
        ParsedRedactionProbe::Command(command) => {
            let lease_id = command.requires_control_lease().then(ControlLeaseId::new);
            let desktop =
                Desktop::for_test(client.clone(), DesktopId::new(), DesktopGeneration::new());
            let submission = desktop
                .prepare_with(CommandId::new(), lease_id, None, *command)
                .map_err(|error| format!("submission construction failed: {error}"))?;
            debug_surfaces.push(format!("{submission:?}"));
            RedactionProbe::Submission(Box::new(submission))
        }
        ParsedRedactionProbe::Viewer(ticket) => {
            debug_surfaces.push(format!("{ticket:?}"));
            RedactionProbe::Status
        }
    };
    let mut fault_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let _request = read_fixture_request(&mut stream).await?;
        drop(stream);
        Ok::<(), std::io::Error>(())
    });
    let mut error_surfaces = Vec::new();
    match probe {
        RedactionProbe::Artifact { content_type, body } => {
            if let Err(error) = client
                .upload_artifact("/v1/artifacts", &content_type, body)
                .await
            {
                error_surfaces.extend([format!("{error}"), format!("{error:?}")]);
            }
        }
        RedactionProbe::Status => {
            if let Err(error) = client.status().await {
                error_surfaces.extend([format!("{error}"), format!("{error:?}")]);
            }
        }
        RedactionProbe::Submission(submission) => {
            if let Err(error) = submission.send().await {
                error_surfaces.extend([format!("{error}"), format!("{error:?}")]);
            }
        }
    }

    if error_surfaces.is_empty() {
        return Err("redaction fixture did not exercise an SDK error surface".to_owned());
    }
    match timeout(Duration::from_secs(2), &mut fault_server).await {
        Ok(result) => result
            .map_err(|_| "redaction fault server task failed".to_owned())?
            .map_err(|error| format!("redaction fault server I/O failed: {error}"))?,
        Err(_) => {
            fault_server.abort();
            let _ = fault_server.await;
            return Err("redaction fault server timed out".to_owned());
        }
    }
    let websocket_url = configured_client.websocket_url();
    Ok(json!({
        "debug_leaked": debug_surfaces.iter().any(|surface| surface.contains(secret)),
        "error_leaked": error_surfaces.iter().any(|surface| surface.contains(secret)),
        "url_leaked": websocket_url.contains(secret),
    }))
}

async fn reference_lifecycle(input: &Value) -> Result<Value, String> {
    let kind = required_text(input, "kind")?;
    if !matches!(kind, "window" | "element") {
        return Err("unsupported reference kind".to_owned());
    }
    let original_wire = input
        .get("original")
        .ok_or_else(|| "original is required".to_owned())?;
    let current_wire = input
        .get("current")
        .ok_or_else(|| "current is required".to_owned())?;
    let relocated_wire = input.get("relocated").filter(|value| !value.is_null());
    let problem_wire = input
        .get("server_problem")
        .ok_or_else(|| "server_problem is required".to_owned())?
        .clone();
    let problem: Problem = parse_wire_value(&problem_wire, "server problem")?;
    problem
        .validate()
        .map_err(|_| "server problem failed validation".to_owned())?;
    let problem_status = problem.status();

    enum ReferenceProbe {
        Window {
            desktop_id: DesktopId,
            desktop_generation: DesktopGeneration,
            token: WindowReferenceToken,
        },
        Element(ElementSnapshotRequest),
    }

    let (probe, stale, identity_unchanged, relocated_distinct) = match kind {
        "window" => {
            let original: WindowRef = parse_wire_value(original_wire, "original window")?;
            let current: WindowRef = parse_wire_value(current_wire, "current window")?;
            original
                .validate()
                .map_err(|_| "original window failed validation".to_owned())?;
            current
                .validate()
                .map_err(|_| "current window failed validation".to_owned())?;
            let relocated = relocated_wire
                .map(|value| parse_wire_value::<WindowRef>(value, "relocated window"))
                .transpose()?;
            if let Some(reference) = &relocated {
                reference
                    .validate()
                    .map_err(|_| "relocated window failed validation".to_owned())?;
            }
            let handle = WindowHandle::from_reference(original.clone())
                .map_err(|error| format!("window handle construction failed: {error}"))?;
            let stale = matches!(
                handle.check_current(&current),
                Err(SdkError::StaleReference)
            );
            let relocated = relocated
                .map(|reference| handle.relocate(reference))
                .transpose()
                .map_err(|error| format!("window relocation failed: {error}"))?;
            let token = WindowReferenceToken::new("conformance_reference_token")
                .map_err(|error| format!("window fixture token failed: {error}"))?;
            (
                ReferenceProbe::Window {
                    desktop_id: original.desktop_id,
                    desktop_generation: original.desktop_generation,
                    token,
                },
                stale,
                handle.reference() == &original,
                relocated
                    .as_ref()
                    .is_some_and(|value| !handle.same_identity(value)),
            )
        }
        "element" => {
            let original: ElementRef = parse_wire_value(original_wire, "original element")?;
            let current: ElementRef = parse_wire_value(current_wire, "current element")?;
            original
                .validate()
                .map_err(|_| "original element failed validation".to_owned())?;
            current
                .validate()
                .map_err(|_| "current element failed validation".to_owned())?;
            let relocated = relocated_wire
                .map(|value| parse_wire_value::<ElementRef>(value, "relocated element"))
                .transpose()?;
            if let Some(reference) = &relocated {
                reference
                    .validate()
                    .map_err(|_| "relocated element failed validation".to_owned())?;
            }
            let handle = ElementHandle::from_reference(original.clone())
                .map_err(|error| format!("element handle construction failed: {error}"))?;
            let stale = matches!(
                handle.check_current(&current),
                Err(SdkError::StaleReference)
            );
            let relocated = relocated
                .map(|reference| handle.relocate(reference))
                .transpose()
                .map_err(|error| format!("element relocation failed: {error}"))?;
            let request = ElementSnapshotRequest {
                desktop_id: original.desktop_id,
                desktop_generation: original.desktop_generation,
                element: original.clone(),
                expansion: ElementSnapshotExpansion::default(),
            };
            (
                ReferenceProbe::Element(request),
                stale,
                handle.reference() == &original,
                relocated
                    .as_ref()
                    .is_some_and(|value| !handle.same_identity(value)),
            )
        }
        _ => unreachable!("reference kind was validated above"),
    };
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("reference fixture bind failed: {error}"))?;
    let base = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|error| format!("reference fixture address failed: {error}"))?
    );
    let client = Client::new(base, TEST_TOKEN)
        .map_err(|error| format!("reference fixture client failed: {error}"))?;
    let mut server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = read_fixture_request(&mut stream).await?;
        write_fixture_response(
            &mut stream,
            TransportFixture {
                kind: "problem".to_owned(),
                status: Some(problem_status),
                body: Some(problem_wire),
                delay_ms: None,
            },
        )
        .await?;
        Ok::<Vec<u8>, std::io::Error>(request)
    });
    let (server_error, expected_path) = match probe {
        ReferenceProbe::Window {
            desktop_id,
            desktop_generation,
            token,
        } => {
            let desktop = Desktop::for_test(client, desktop_id, desktop_generation);
            let server_error = match desktop.windows().snapshot(&token).await {
                Err(error) => error,
                Ok(_) => return Err("window stale-reference fixture unexpectedly succeeded".into()),
            };
            (
                server_error,
                format!(
                    "/v1/desktops/{desktop_id}/windows/{}?desktop_generation={desktop_generation}",
                    token.as_str()
                ),
            )
        }
        ReferenceProbe::Element(request) => {
            let desktop = Desktop::for_test(client, request.desktop_id, request.desktop_generation);
            let server_error = match desktop.accessibility().snapshot(&request).await {
                Err(error) => error,
                Ok(_) => {
                    return Err("element stale-reference fixture unexpectedly succeeded".to_owned());
                }
            };
            (
                server_error,
                format!(
                    "/v1/desktops/{}/accessibility/elements/snapshot",
                    request.desktop_id
                ),
            )
        }
    };
    let request = match timeout(Duration::from_secs(2), &mut server).await {
        Ok(result) => result
            .map_err(|_| "reference fixture server task failed".to_owned())?
            .map_err(|error| format!("reference fixture server I/O failed: {error}"))?,
        Err(_) => {
            server.abort();
            let _ = server.await;
            return Err("reference fixture server timed out".to_owned());
        }
    };
    let request_line = String::from_utf8(request)
        .map_err(|_| "reference fixture request was not UTF-8".to_owned())?
        .lines()
        .next()
        .ok_or_else(|| "reference fixture request line was missing".to_owned())?
        .to_owned();
    let transport_exercised = request_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|path| path == expected_path);
    if !transport_exercised {
        return Err(format!(
            "reference fixture observed unexpected request {request_line}"
        ));
    }
    let server_error_code = server_error
        .problem()
        .ok_or_else(|| "server problem was not retained by the SDK error".to_owned())?
        .code();

    Ok(json!({
        "stale": server_error_code == xenoteer_protocol::ErrorCode::StaleReference
            && stale,
        "server_error_code": enum_text(server_error_code)?,
        "identity_unchanged": identity_unchanged,
        "relocated_distinct": relocated_distinct,
        "generation_changed": stale,
        "transport_exercised": transport_exercised,
    }))
}

fn classify_terminal_effect(input: &Value) -> Result<Value, String> {
    let result: CommandResult = parse_wire_value(
        input
            .get("result")
            .ok_or_else(|| "result is required".to_owned())?,
        "command result",
    )?;
    result
        .validate()
        .map_err(|_| "terminal result failed validation".to_owned())?;
    let encoded =
        serde_json::to_value(&result).map_err(|_| "result serialization failed".to_owned())?;
    let problem = result.error();
    Ok(json!({
        "lifecycle": enum_text(result.lifecycle())?,
        "effect_stage": enum_text(result.effect_stage())?,
        "has_visible_effect": result.effect_stage().has_visible_effect(),
        "error_code": problem.map(Problem::code).map(enum_text).transpose()?,
        "retry": problem.map(Problem::retry).map(enum_text).transpose()?,
        "details": problem.map_or_else(|| json!({}), |value| json!(value.details())),
        "warning_count": encoded
            .get("warnings")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        "outcome_type": encoded.pointer("/outcome/type").cloned().unwrap_or(Value::Null),
    }))
}

fn event_configuration(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
) -> Result<EventConfiguration, String> {
    let client = Client::new("http://127.0.0.1:9", TEST_TOKEN)
        .map_err(|error| format!("event client failed: {error}"))?;
    Ok(EventConfiguration {
        cancellation: client.cancellation_token(),
        client,
        desktop_id,
        desktop_generation: generation,
        protocol: ProtocolVersion::V1_0,
        topics: Arc::new(Vec::new()),
    })
}

fn event_continuity(input: &Value) -> Result<Value, String> {
    let desktop_id: DesktopId = required_wire(input, "desktop_id")?;
    let desktop_generation: DesktopGeneration = required_wire(input, "desktop_generation")?;
    let _cursor_generation: DesktopGeneration = required_wire(input, "cursor_generation")?;
    let request_id: RequestId = required_wire(input, "subscription_request_id")?;
    let topics = input
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| "topics must be an array".to_owned())?
        .iter()
        .map(|value| parse_wire_value::<EventTopic>(value, "event topic"))
        .collect::<Result<Vec<_>, _>>()?;
    let queue_capacity = input
        .get("queue_capacity")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| "queue_capacity must be positive".to_owned())?;
    let mut cursor = input
        .get("initial_cursor")
        .filter(|value| !value.is_null())
        .map(|value| parse_wire_u64(value, "initial_cursor"))
        .transpose()?;
    let frames = input
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(|| "frames must be an array".to_owned())?;

    let mut configuration = event_configuration(desktop_id, desktop_generation)?;
    configuration.topics = Arc::new(topics);
    let (sender, mut receiver) = mpsc::channel(queue_capacity.saturating_add(2));
    let mut terminal = None;
    let mut generation_changed = false;
    for frame in frames {
        let encoded =
            serde_json::to_string(frame).map_err(|_| "event frame did not encode".to_owned())?;
        if let Err(reason) = handle_server_text(
            &configuration,
            &sender,
            queue_capacity,
            &encoded,
            request_id,
            &mut cursor,
        ) {
            generation_changed = matches!(
                &reason,
                EventStreamCloseReason::InvalidMessage {
                    generation_changed: true
                }
            );
            terminal = Some(close_reason_text(&reason));
            if reason == EventStreamCloseReason::QueueOverflow {
                let _terminal = sender.try_send(EventStreamItem::Closed { reason });
            }
            break;
        }
    }
    drop(sender);

    let mut delivered_sequences = Vec::new();
    let mut resync_reason = None;
    while let Ok(item) = receiver.try_recv() {
        match item {
            EventStreamItem::Event(event) => delivered_sequences.push(event.sequence.to_string()),
            EventStreamItem::ResyncRequired { reason, .. } => {
                generation_changed |=
                    matches!(reason, Some(EventStreamResyncReason::GenerationChanged));
                resync_reason = reason.map(enum_text).transpose()?;
            }
            EventStreamItem::Closed { reason } => {
                terminal.get_or_insert_with(|| close_reason_text(&reason));
            }
            _ => {}
        }
    }
    let refresh_required = matches!(
        terminal.as_deref(),
        Some("invalid_message" | "queue_overflow" | "resync_required")
    );
    Ok(json!({
        "delivered_sequences": delivered_sequences,
        "final_cursor": cursor.map(|value| value.to_string()),
        "terminal": terminal,
        "resync_reason": resync_reason,
        "refresh_required": refresh_required,
        "generation_changed": generation_changed,
    }))
}

#[derive(Clone, Deserialize)]
struct TransportFixture {
    kind: String,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    delay_ms: Option<u64>,
}

async fn read_fixture_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<Vec<u8>, std::io::Error> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.len() > 256 * 1024 {
            return Err(std::io::Error::other("fixture request too large"));
        }
        let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if request.len() >= header_end.saturating_add(content_length) {
            break;
        }
    }
    Ok(request)
}

async fn write_fixture_response(
    stream: &mut tokio::net::TcpStream,
    response: TransportFixture,
) -> Result<(), std::io::Error> {
    match response.kind.as_str() {
        "disconnect" => Ok(()),
        "stall" => {
            sleep(Duration::from_millis(response.delay_ms.unwrap_or(1))).await;
            Ok(())
        }
        "json" | "problem" => {
            let status = response
                .status
                .ok_or_else(|| std::io::Error::other("fixture status missing"))?;
            let body = serde_json::to_vec(
                response
                    .body
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("fixture body missing"))?,
            )
            .map_err(std::io::Error::other)?;
            let reason = match status {
                200 => "OK",
                202 => "Accepted",
                404 => "Not Found",
                409 => "Conflict",
                _ => "Fixture",
            };
            let content_type = if response.kind == "problem" {
                "application/problem+json"
            } else {
                "application/json"
            };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await
        }
        _ => Err(std::io::Error::other("unsupported fixture response kind")),
    }
}

fn validate_submission_identity(
    requests: &[String],
    command_id: CommandId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<Vec<Value>, String> {
    let mut submissions = 0_usize;
    let mut envelopes = Vec::new();
    for request in requests {
        let Some(line) = request.lines().next() else {
            continue;
        };
        if !line.starts_with("POST ")
            || !line
                .split_whitespace()
                .nth(1)
                .is_some_and(|path| path.ends_with("/commands"))
        {
            continue;
        }
        submissions += 1;
        let expected_header = format!("idempotency-key: {command_id}");
        if !request
            .to_ascii_lowercase()
            .contains(&expected_header.to_ascii_lowercase())
        {
            return Err("submission omitted the exact Idempotency-Key".to_owned());
        }
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .ok_or_else(|| "submission omitted its JSON body".to_owned())?;
        let wire: Value =
            serde_json::from_str(body).map_err(|_| "submission body was not JSON".to_owned())?;
        let envelope: CommandEnvelope = parse_wire_value(&wire, "submitted command envelope")?;
        envelope
            .validate()
            .map_err(|_| "submitted command envelope failed validation".to_owned())?;
        if envelope.command_id != command_id
            || envelope.desktop_id != desktop_id
            || envelope.desktop_generation != desktop_generation
            || envelope.protocol_version != ProtocolVersion::V1_0
            || envelope.lease_id.is_some()
            || envelope.deadline.is_some()
            || envelope.trace_policy.is_some()
        {
            return Err("submission envelope changed fixed scenario fields".to_owned());
        }
        envelopes.push(json!({
            "protocol_version": envelope.protocol_version,
            "request_id_non_nil": !envelope.request_id.as_uuid().is_nil(),
            "command_id": envelope.command_id,
            "desktop_id": envelope.desktop_id,
            "desktop_generation": envelope.desktop_generation,
            "lease_id": envelope.lease_id,
            "deadline": envelope.deadline,
            "trace_policy": envelope.trace_policy,
            "command": envelope.command,
        }));
    }
    if submissions == 0 {
        return Err("command scenario made no submission".to_owned());
    }
    Ok(envelopes)
}

async fn command_reconnect(input: &Value) -> Result<Value, String> {
    let desktop_id: DesktopId = required_wire(input, "desktop_id")?;
    let desktop_generation: DesktopGeneration = required_wire(input, "desktop_generation")?;
    let reconnect_generation: DesktopGeneration = required_wire(input, "reconnect_generation")?;
    let command_id: CommandId = required_wire(input, "command_id")?;
    let command: Command = required_wire(input, "command")?;
    let initial_response: TransportFixture = required_wire(input, "initial_response")?;
    let lookup_response = optional_wire::<TransportFixture>(input, "lookup_response")?;
    let resubmit_command = optional_wire::<Command>(input, "resubmit_command")?;
    let resubmit_response = optional_wire::<TransportFixture>(input, "resubmit_response")?;
    let cancel_after_ms = input
        .get("cancel_after_ms")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or_else(|| "cancel_after_ms must be positive".to_owned())
        })
        .transpose()?;
    let mut responses = vec![initial_response];
    if let Some(response) = lookup_response {
        responses.push(response);
    }
    if let Some(response) = resubmit_response {
        responses.push(response);
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("fault listener failed: {error}"))?;
    let base = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|error| format!("fault listener address failed: {error}"))?
    );
    let observed_requests = Arc::new(Mutex::new(Vec::new()));
    let server_requests = observed_requests.clone();
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await?;
            let request = read_fixture_request(&mut stream).await?;
            server_requests
                .lock()
                .map_err(|_| std::io::Error::other("request log lock failed"))?
                .push(String::from_utf8_lossy(&request).into_owned());
            write_fixture_response(&mut stream, response).await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let desktop = Desktop::for_test(
        Client::new(base, TEST_TOKEN).map_err(|error| format!("fault client failed: {error}"))?,
        desktop_id,
        desktop_generation,
    );
    let submission = desktop
        .prepare_with(command_id, None, None, command)
        .map_err(|error| format!("submission preparation failed: {error}"))?;
    let initial = if let Some(delay) = cancel_after_ms {
        timeout(Duration::from_millis(delay), submission.send())
            .await
            .ok()
            .transpose()
            .map_err(|error| format!("initial submission failed unexpectedly: {error}"))?
    } else {
        submission.send().await.ok()
    };

    let mut observed_result = initial.as_ref().map(|handle| handle.latest().clone());
    let mut outcome = if initial.is_some() {
        "submitted".to_owned()
    } else {
        "transport_ambiguous".to_owned()
    };
    let mut error_code = None;
    if let Err(error) = submission.ensure_generation(reconnect_generation) {
        if !matches!(error, SdkError::GenerationChanged) {
            return Err(format!("unexpected generation-fence failure: {error}"));
        }
        outcome = "stale_generation".to_owned();
        error_code = Some("generation_changed".to_owned());
    } else if initial.is_none() {
        if input
            .get("lookup_response")
            .is_some_and(|value| !value.is_null())
        {
            match desktop.command(command_id).await {
                Ok(handle) => {
                    observed_result = Some(handle.latest().clone());
                    outcome = "reattached".to_owned();
                }
                Err(error)
                    if error.problem().is_some_and(|problem| {
                        problem.code() == xenoteer_protocol::ErrorCode::NotFound
                    }) => {}
                Err(error) => return Err(format!("command lookup failed unexpectedly: {error}")),
            }
        }
        if observed_result.is_none()
            && let Some(command) = resubmit_command
        {
            let resubmission = desktop
                .prepare_with(command_id, None, None, command)
                .map_err(|error| format!("resubmission preparation failed: {error}"))?;
            match resubmission.send().await {
                Ok(handle) => {
                    observed_result = Some(handle.latest().clone());
                    outcome = "resubmitted".to_owned();
                }
                Err(error) => {
                    if let Some(problem) = error.problem() {
                        error_code = Some(enum_text(problem.code())?);
                        outcome = error_code
                            .clone()
                            .unwrap_or_else(|| "server_problem".to_owned());
                    } else {
                        return Err(format!("resubmission failed unexpectedly: {error}"));
                    }
                }
            }
        }
    }

    timeout(Duration::from_secs(2), server)
        .await
        .map_err(|_| "fault server timed out".to_owned())?
        .map_err(|_| "fault server task failed".to_owned())?
        .map_err(|error| format!("fault server I/O failed: {error}"))?;

    if submission.id() != command_id {
        return Err("submission changed the caller-selected command ID".to_owned());
    }
    let requests = observed_requests
        .lock()
        .map_err(|_| "request log lock failed".to_owned())?;
    let submission_envelopes =
        validate_submission_identity(&requests, command_id, desktop_id, desktop_generation)?;
    let request_lines = requests
        .iter()
        .filter_map(|request| request.lines().next())
        .collect::<Vec<_>>();
    let submission_attempts = request_lines
        .iter()
        .filter(|line| {
            line.starts_with("POST ")
                && line
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|path| path.ends_with("/commands"))
        })
        .count();
    let lookup_attempts = request_lines
        .iter()
        .filter(|line| {
            line.starts_with("GET ")
                && line
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|path| path.contains(&format!("/commands/{command_id}")))
        })
        .count();
    let cancel_requests = request_lines
        .iter()
        .filter(|line| line.contains("/cancel "))
        .count();
    Ok(json!({
        "outcome": outcome,
        "command_id": command_id,
        "submission_attempts": submission_attempts,
        "lookup_attempts": lookup_attempts,
        "cancel_requests": cancel_requests,
        "submission_envelopes": submission_envelopes,
        "lifecycle": observed_result
            .as_ref()
            .map(|result| enum_text(result.lifecycle()))
            .transpose()?,
        "error_code": error_code,
    }))
}

async fn runtime_lifecycle_self_check() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::new("http://127.0.0.1:9", TEST_TOKEN)?;
    let clone = client.clone();
    client.close().await;
    if !clone.is_closed() || !matches!(clone.status().await, Err(SdkError::ClientClosed)) {
        return Err("clone-wide client close invariant failed".into());
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0_u8; 4096];
        let _read = stream.read(&mut request).await?;
        drop(stream);
        Ok::<(), std::io::Error>(())
    });
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let lease_id = ControlLeaseId::new();
    let desktop = Desktop::for_test(Client::new(base, TEST_TOKEN)?, desktop_id, generation);
    let state = LeaseStateView {
        desktop_id,
        desktop_generation: generation,
        state: LeaseAvailability::HeldByCaller,
        lease_id: Some(lease_id),
        expires_at: Some(Timestamp::parse("2030-01-01T00:01:00Z")?),
    };
    let mut lease = ControlLease::from_acquire(desktop, Some(30_000), state)?;
    if !matches!(lease.release().await, Err(SdkError::Transport))
        || !lease.is_active()
        || lease.id() != lease_id
    {
        return Err("ambiguous lease release invariant failed".into());
    }
    server.await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::{
        io::AsyncReadExt,
        net::TcpListener,
        time::{Duration, timeout},
    };

    use super::*;

    #[tokio::test]
    async fn redaction_uses_an_internal_deterministic_failure_transport()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let supplied_base = format!("http://{}", listener.local_addr()?);
        let received = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&received);
        let server = tokio::spawn(async move {
            if let Ok(Ok((mut stream, _))) =
                timeout(Duration::from_millis(250), listener.accept()).await
            {
                observed.store(true, Ordering::SeqCst);
                let mut request = [0_u8; 4096];
                let _read = stream.read(&mut request).await?;
            }
            Ok::<(), std::io::Error>(())
        });
        let actual = redaction(&json!({
            "kind": "bearer",
            "secret": "XENOTEER_BEARER_CANARY_76e3da9955c6",
            "base_url": supplied_base,
            "raw": {
                "authorization": "Bearer XENOTEER_BEARER_CANARY_76e3da9955c6"
            }
        }))
        .await?;
        assert_eq!(
            actual,
            json!({
                "debug_leaked": false,
                "error_leaked": false,
                "url_leaked": false
            })
        );
        server.await??;
        assert!(
            !received.load(Ordering::SeqCst),
            "redaction proof depended on the caller-supplied network endpoint"
        );
        Ok(())
    }

    #[tokio::test]
    async fn redaction_rejects_unknown_kind_before_starting_a_fixture() {
        let result = redaction(&json!({"kind": "unknown"})).await;
        assert_eq!(
            result.as_ref().map_err(String::as_str),
            Err("unsupported redaction kind")
        );
    }

    #[tokio::test]
    async fn reference_lifecycle_exercises_the_sdk_transport()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../conformance/v1/cases/stale-references.json"
        ))?;
        let cases = fixture
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("stale-reference fixture has no cases")?;
        for case in cases {
            let input = case
                .get("input")
                .ok_or("stale-reference fixture case has no input")?;
            let actual = reference_lifecycle(input).await?;
            assert_eq!(
                actual.get("transport_exercised"),
                Some(&Value::Bool(true)),
                "reference lifecycle must observe the server problem through a public SDK operation"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn reference_lifecycle_rejects_unknown_kind_before_starting_a_fixture() {
        let result = reference_lifecycle(&json!({"kind": "unknown"})).await;
        assert_eq!(
            result.as_ref().map_err(String::as_str),
            Err("unsupported reference kind")
        );
    }
}
