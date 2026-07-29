use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    AccessibilityEvent, ApplicationRef, ArtifactRef, CapabilityReport, ClientHello,
    ClipboardReadRequest, ClipboardReadResult, ClipboardWriteSource, CommandEnvelope,
    CommandResult, ElementListPage, ElementListRequest, ElementQueryPage, ElementQueryRequest,
    ElementRef, ElementResolveRequest, ElementResolveResult, ElementSnapshotRequest,
    ElementSnapshotResult, ElementWaitRequest, ElementWaitResult, EventTopic, LeaseAcquireRequest,
    LeaseReleaseRequest, LeaseRenewRequest, LeaseStateView, OneTimeViewerTicket, Problem,
    ProcessExitedEvent, ProcessRef, ProcessTerminateCommand, ProcessView, ScreenshotRequest,
    ScreenshotResult, TextInsertCommand, TextSource, ViewerSessionEvidence, ViewerTicketRequest,
    WebSocketClientMessage, WebSocketServerMessage, WindowGeometryPredicate, WindowListPage,
    WindowListRequest, WindowManagerCapabilities, WindowMoveResizeCommand, WindowQueryPage,
    WindowQueryRequest, WindowResolveRequest, WindowResolveResult, WindowSnapshotRequest,
    WindowSnapshotResult, WindowWaitRequest, WindowWaitResult,
};

fn assert_accepts_additive_field<T>(mut value: Value)
where
    T: DeserializeOwned,
{
    assert!(value.is_object(), "test fixture must be an object");
    if let Some(object) = value.as_object_mut() {
        object.insert("future_additive_field".to_owned(), json!({ "version": 2 }));
    }
    assert!(
        serde_json::from_value::<T>(value).is_ok(),
        "{} rejected an additive response field",
        core::any::type_name::<T>()
    );
}

fn assert_rejects_additive_field<T>(mut value: Value)
where
    T: DeserializeOwned,
{
    assert!(value.is_object(), "test fixture must be an object");
    if let Some(object) = value.as_object_mut() {
        object.insert("future_additive_field".to_owned(), json!(true));
    }
    assert!(
        serde_json::from_value::<T>(value).is_err(),
        "{} accepted an additive request field",
        core::any::type_name::<T>()
    );
}

fn assert_accepts_nested_addition<T>(mut value: Value, pointer: &str)
where
    T: DeserializeOwned,
{
    assert!(
        value.pointer(pointer).is_some_and(Value::is_object),
        "test fixture pointer must identify an object"
    );
    if let Some(object) = value.pointer_mut(pointer).and_then(Value::as_object_mut) {
        object.insert("future_nested_field".to_owned(), json!({ "version": 2 }));
    }
    assert!(
        serde_json::from_value::<T>(value).is_ok(),
        "{} rejected additive output at {pointer}",
        core::any::type_name::<T>()
    );
}

fn generation() -> &'static str {
    "018f1e74-7a6b-7cc0-8000-000000000001"
}

fn desktop() -> &'static str {
    "018f1e74-7a6b-7cc0-8000-000000000002"
}

fn request() -> &'static str {
    "018f1e74-7a6b-7cc0-8000-000000000003"
}

fn process_reference() -> Value {
    json!({
        "desktop_generation": generation(),
        "pid": 42,
        "proc_start_ticks": "99",
        "launch_id": "018f1e74-7a6b-7cc0-8000-000000000004"
    })
}

fn artifact_reference() -> Value {
    json!({
        "artifact_id": "018f1e74-7a6b-7cc0-8000-000000000005",
        "purpose": "screenshot",
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "content_type": "image/png",
        "content_length": 4,
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "created_at": "2026-07-21T00:00:00Z",
        "expires_at": "2026-07-21T00:01:00Z"
    })
}

fn screenshot_result() -> Value {
    json!({
        "target": { "kind": "root" },
        "source_region": {
            "coordinate_space": "root_physical",
            "rect": { "x": 0, "y": 0, "width": 1, "height": 1 }
        },
        "source_size": { "width": 1, "height": 1 },
        "limitation": "root_visible_framebuffer",
        "format": "png",
        "size": { "width": 1, "height": 1 },
        "raw": null,
        "cursor": {
            "requested": false,
            "composited": false,
            "serial_before": null,
            "serial_after": null,
            "moved_during_capture": false
        },
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "delivery": { "delivery": "inline_body", "content_length": 4 }
    })
}

fn clipboard_read_result() -> Value {
    json!({
        "selection": "clipboard",
        "revision": "1",
        "evidence": {
            "target": "UTF8_STRING",
            "transfer": { "mode": "direct" },
            "content_length": 0,
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "owner_changed": false,
            "terminal_chunk_observed": false,
            "terminal": { "status": "completed" }
        },
        "content": { "delivery": "inline_text", "text": "" }
    })
}

fn window_reference() -> Value {
    json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "xid": 42,
        "observed_generation": "1",
        "identity_hash": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    })
}

fn window_snapshot() -> Value {
    json!({
        "ref": window_reference(),
        "xid_hex": "0x0000002a",
        "model_revision": "7",
        "metadata": {
            "title": null,
            "visible_title": null,
            "icon_title": null,
            "class": null,
            "client_machine": null,
            "window_types": [],
            "states": [],
            "allowed_actions": [],
            "protocols": []
        },
        "process": {
            "reported_pid": null,
            "managed_process": null,
            "confidence": "none",
            "evidence": [],
            "conflict": false
        },
        "state": {
            "map_state": "viewable",
            "minimized": false,
            "hidden": false,
            "urgent": false,
            "modal": false,
            "sticky": false,
            "active": false,
            "focused": false
        },
        "geometry": null,
        "workspace": 0,
        "client_leader": null,
        "transient_for": null,
        "group_leader": null,
        "stacking_index": 0,
        "has_accessibility_application": false,
        "warnings": []
    })
}

fn window_entry() -> Value {
    json!({
        "snapshot": window_snapshot(),
        "reference_token": "A_window_reference_1"
    })
}

fn application_reference() -> Value {
    json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "unique_bus_name": ":1.42",
        "root_object_path": "/org/a11y/atspi/accessible/root",
        "app_instance_generation": "1",
        "identity_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    })
}

fn element_reference() -> Value {
    json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "application": application_reference(),
        "object_path": "/org/a11y/atspi/accessible/42",
        "object_identity_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cache_sequence": "7"
    })
}

fn element_snapshot() -> Value {
    json!({
        "ref": element_reference(),
        "parent": null,
        "index_in_parent": 0,
        "child_count": 0,
        "role": { "role": "button", "raw_name": "push button", "raw_numeric": 42 },
        "name": "Save",
        "description": null,
        "accessible_id": "save-button",
        "locale": "en-US",
        "states": ["enabled", "sensitive", "visible", "showing"],
        "interfaces": ["accessible", "action", "component"],
        "actions": [{ "name": "click", "description": "Activate", "key_binding": null }],
        "value": null,
        "text": null,
        "component": {
            "coordinate_space": "root_physical",
            "extents": { "x": 1, "y": 2, "width": 100, "height": 20 },
            "layer": "widget",
            "z_order": 0,
            "alpha": 255
        },
        "attributes": [],
        "relations": [],
        "window_correlation": {
            "window": null,
            "confidence": "none",
            "evidence": [],
            "conflicting_evidence": false
        },
        "revision": "3",
        "completeness": "complete",
        "truncated": false,
        "warnings": []
    })
}

fn element_page() -> Value {
    json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "snapshot_revision": "3",
        "order": "preorder",
        "elements": [{ "snapshot": element_snapshot() }],
        "next_cursor": null,
        "visited_nodes": 1,
        "truncated": false,
        "warnings": []
    })
}

fn element_selector() -> Value {
    json!({
        "scope": { "type": "desktop" },
        "predicates": [{
            "type": "role",
            "roles": ["button"]
        }],
        "order": "preorder",
        "result_index": null
    })
}

fn element_expansion() -> Value {
    json!({
        "actions": false,
        "value": false,
        "text_metadata": false,
        "text_content": false,
        "attributes": false,
        "relations": false,
        "component": true
    })
}

fn accessibility_limits() -> Value {
    json!({
        "max_visited_nodes": 25000,
        "max_depth": 64,
        "max_matches": 1000,
        "timeout_ms": 10000
    })
}

fn selector() -> Value {
    json!({
        "type": "predicate",
        "predicate": { "type": "active", "value": true }
    })
}

fn command_envelope() -> Value {
    json!({
        "protocol_version": { "major": 1, "minor": 0 },
        "request_id": request(),
        "command_id": "018f1e74-7a6b-7cc0-8000-000000000006",
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "lease_id": null,
        "deadline": null,
        "trace_policy": null,
        "command": { "type": "desktop_probe" }
    })
}

fn accepted_command_result() -> Value {
    json!({
        "command_id": "018f1e74-7a6b-7cc0-8000-000000000006",
        "lifecycle": "accepted",
        "effect_stage": "accepted",
        "accepted_at": "2026-07-21T00:00:00Z",
        "started_at": null,
        "finished_at": null,
        "outcome": null,
        "error": null,
        "warnings": []
    })
}

fn succeeded_process_result() -> Value {
    json!({
        "command_id": "018f1e74-7a6b-7cc0-8000-000000000006",
        "lifecycle": "succeeded",
        "effect_stage": "process_started",
        "accepted_at": "2026-07-21T00:00:00Z",
        "started_at": "2026-07-21T00:00:01Z",
        "finished_at": "2026-07-21T00:00:02Z",
        "outcome": {
            "type": "application_launched",
            "process": process_reference()
        },
        "error": null,
        "warnings": []
    })
}

fn succeeded_element_action_result() -> Value {
    json!({
        "command_id": "018f1e74-7a6b-7cc0-8000-000000000006",
        "lifecycle": "succeeded",
        "effect_stage": "semantic_state_changed",
        "accepted_at": "2026-07-21T00:00:00Z",
        "started_at": "2026-07-21T00:00:01Z",
        "finished_at": "2026-07-21T00:00:02Z",
        "outcome": {
            "type": "element_action",
            "result": {
                "operation": "invoke",
                "element": element_reference(),
                "revision_before": "3",
                "revision_after": "4",
                "snapshot_before": null,
                "snapshot_after": null,
                "evidence": {
                    "resolved_action_name": "click",
                    "resolved_action_index": 0,
                    "backend_accepted": true,
                    "observed_state": null,
                    "observed_value": null,
                    "observed_selection_count": null,
                    "observed_text_length": null,
                    "protected_text_verified_by_length_only": false,
                    "extents_before": null,
                    "extents_after": null,
                    "postcondition_satisfied": null,
                    "poll_fallback_used": false
                }
            }
        },
        "error": null,
        "warnings": []
    })
}

fn succeeded_semantic_text_result() -> Value {
    json!({
        "command_id": "018f1e74-7a6b-7cc0-8000-000000000006",
        "lifecycle": "succeeded",
        "effect_stage": "text_inserted",
        "accepted_at": "2026-07-21T00:00:00Z",
        "started_at": "2026-07-21T00:00:01Z",
        "finished_at": "2026-07-21T00:00:02Z",
        "outcome": {
            "type": "text_inserted",
            "evidence": {
                "selected_strategy": "semantic",
                "utf8_bytes": 3,
                "unicode_scalars": 3,
                "completed_scalars": 3,
                "clipboard": null,
                "semantic": {
                    "element": element_reference(),
                    "revision_before": "3",
                    "revision_after": "4",
                    "backend_accepted": true,
                    "insertion_offset": 0,
                    "character_count_before": 0,
                    "character_count_after": 3,
                    "caret_offset_after": 3,
                    "selection_count_after": 0,
                    "verified_length_only": true,
                    "postcondition_satisfied": true
                }
            }
        },
        "error": null,
        "warnings": []
    })
}

#[test]
fn precision_sensitive_counters_are_strings_while_bounded_lengths_remain_numbers() {
    let process = process_reference();
    assert!(
        serde_json::from_value::<ProcessRef>(process.clone()).is_ok(),
        "canonical process counter string must decode"
    );
    let mut numeric_process = process;
    numeric_process["proc_start_ticks"] = json!(99);
    assert!(
        serde_json::from_value::<ProcessRef>(numeric_process).is_err(),
        "JSON numbers must not enter precision-sensitive counters"
    );

    let artifact = artifact_reference();
    assert!(
        serde_json::from_value::<ArtifactRef>(artifact.clone()).is_ok(),
        "bounded artifact lengths remain JSON numbers"
    );
    let mut string_length = artifact;
    string_length["content_length"] = json!("4");
    assert!(
        serde_json::from_value::<ArtifactRef>(string_length).is_err(),
        "bounded artifact lengths must not silently change wire type"
    );

    let subscription = json!({
        "type": "events.subscribe",
        "request_id": request(),
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "topics": [],
        "since_sequence": "9007199254740993"
    });
    assert!(
        serde_json::from_value::<WebSocketClientMessage>(subscription.clone()).is_ok(),
        "event cursors beyond JavaScript's safe integer range must round trip exactly"
    );
    let mut numeric_subscription = subscription;
    numeric_subscription["since_sequence"] = json!(1);
    assert!(serde_json::from_value::<WebSocketClientMessage>(numeric_subscription).is_err());
}

#[test]
fn every_public_response_family_tolerates_top_level_additions() {
    assert_accepts_additive_field::<CapabilityReport>(json!({ "capabilities": [] }));
    assert_accepts_additive_field::<CommandResult>(accepted_command_result());
    assert_accepts_additive_field::<Problem>(json!({
        "type": "https://xenoteer.dev/problems/invalid-request",
        "title": "Invalid request",
        "status": 400,
        "code": "invalid_request",
        "detail": "The request is invalid.",
        "instance": null,
        "retry": "never",
        "effect_stage": "none",
        "desktop_generation": null,
        "details": {}
    }));
    assert_accepts_additive_field::<LeaseStateView>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "state": "vacant",
        "lease_id": null,
        "expires_at": null
    }));
    assert_accepts_additive_field::<ProcessView>(json!({
        "process": process_reference(),
        "state": "running",
        "exit": null
    }));
    assert_accepts_additive_field::<ProcessRef>(process_reference());
    assert_accepts_additive_field::<ProcessExitedEvent>(json!({
        "application": "recorder.x11",
        "process": {
            "process": process_reference(),
            "state": "exited",
            "exit": { "code": 0, "signal": null, "core_dumped": false }
        },
        "termination_requested": false,
        "forced_escalation": false
    }));
    assert_accepts_additive_field::<WebSocketServerMessage>(json!({
        "type": "server.pong",
        "request_id": request(),
        "nonce": "n"
    }));
    assert_accepts_additive_field::<ArtifactRef>(artifact_reference());
    assert_accepts_additive_field::<ScreenshotResult>(screenshot_result());
    assert_accepts_additive_field::<ClipboardReadResult>(clipboard_read_result());
    let page = json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "snapshot_revision": "7",
        "windows": [window_entry()],
        "next_cursor": null
    });
    assert_accepts_additive_field::<WindowListPage>(page.clone());
    assert_accepts_additive_field::<WindowQueryPage>(page);
    assert_accepts_additive_field::<WindowSnapshotResult>(json!({
        "snapshot_revision": "7",
        "window": window_entry()
    }));
    assert_accepts_additive_field::<WindowResolveResult>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "snapshot_revision": "7",
        "window": window_entry()
    }));
    assert_accepts_additive_field::<WindowWaitResult>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "status": "matched",
        "evaluated_revision": "7",
        "predicate_satisfied": true,
        "matched_count": 1,
        "windows": [window_entry()]
    }));
    assert_accepts_additive_field::<WindowManagerCapabilities>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "model_revision": "7",
        "supported": []
    }));
    assert_accepts_additive_field::<OneTimeViewerTicket>(json!({
        "ticket": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "principal_id": "viewer-principal",
        "audience": "viewer_websocket",
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "origin": "https://viewer.example",
        "mode": "view_only",
        "issued_at": "2026-07-21T00:00:00Z",
        "expires_at": "2026-07-21T00:01:00Z",
        "use_policy": "single_use"
    }));
    assert_accepts_additive_field::<ViewerSessionEvidence>(json!({
        "principal_id": "viewer-principal",
        "audience": "viewer_websocket",
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "origin": "https://viewer.example",
        "mode": "view_only",
        "ticket_consumed": true,
        "backend_state": "available",
        "established_at": "2026-07-21T00:00:00Z",
        "ended_at": null,
        "end_reason": null
    }));
    assert_accepts_additive_field::<ApplicationRef>(application_reference());
    assert_accepts_additive_field::<ElementRef>(element_reference());
    assert_accepts_additive_field::<ElementListPage>(element_page());
    assert_accepts_additive_field::<ElementQueryPage>(element_page());
    assert_accepts_additive_field::<ElementSnapshotResult>(json!({
        "snapshot_revision": "3",
        "element": { "snapshot": element_snapshot() }
    }));
    assert_accepts_additive_field::<ElementResolveResult>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "snapshot_revision": "3",
        "element": { "snapshot": element_snapshot() }
    }));
    assert_accepts_additive_field::<ElementWaitResult>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "status": "matched",
        "evaluated_revision": "3",
        "predicate_satisfied": true,
        "matched_count": 1,
        "elements": [{ "snapshot": element_snapshot() }],
        "poll_fallback_used": false,
        "truncated": false,
        "warnings": []
    }));
    assert_accepts_additive_field::<AccessibilityEvent>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "source": element_reference(),
        "raw_source": {
            "bus_name": ":1.42",
            "object_path": "/org/a11y/atspi/accessible/42"
        },
        "kind": "state_changed",
        "resync_reason": null,
        "detail": {
            "property": null,
            "state": "focused",
            "enabled": true,
            "child": null,
            "text": null,
            "value": null,
            "bounds": null
        },
        "revision": "4",
        "cache_sequence": "8",
        "source_stale": false
    }));
}

#[test]
fn shared_nested_objects_are_lenient_only_in_responses() {
    let mut response = json!({
        "process": process_reference(),
        "state": "running",
        "exit": null
    });
    response["process"]["future_process_identity"] = json!("pidfd-v2");
    assert!(serde_json::from_value::<ProcessView>(response).is_ok());

    let mut request_value = json!({
        "process": process_reference(),
        "grace_ms": 500
    });
    request_value["process"]["future_process_identity"] = json!("pidfd-v2");
    assert!(serde_json::from_value::<ProcessTerminateCommand>(request_value).is_err());
}

#[test]
fn response_families_tolerate_relevant_nested_additions() {
    assert_accepts_nested_addition::<CapabilityReport>(
        json!({
            "capabilities": [{
                "id": "capture.screenshot",
                "status": "available",
                "reason_code": null,
                "backend_version": null
            }]
        }),
        "/capabilities/0",
    );
    for pointer in ["/outcome", "/outcome/process"] {
        assert_accepts_nested_addition::<CommandResult>(succeeded_process_result(), pointer);
    }
    for pointer in [
        "/outcome",
        "/outcome/result",
        "/outcome/result/element",
        "/outcome/result/element/application",
        "/outcome/result/evidence",
    ] {
        assert_accepts_nested_addition::<CommandResult>(succeeded_element_action_result(), pointer);
    }
    for pointer in [
        "/outcome",
        "/outcome/evidence",
        "/outcome/evidence/semantic",
        "/outcome/evidence/semantic/element",
        "/outcome/evidence/semantic/element/application",
    ] {
        assert_accepts_nested_addition::<CommandResult>(succeeded_semantic_text_result(), pointer);
    }
    let exited = json!({
        "application": "recorder.x11",
        "process": {
            "process": process_reference(),
            "state": "exited",
            "exit": { "code": 0, "signal": null, "core_dumped": false }
        },
        "termination_requested": false,
        "forced_escalation": false
    });
    for pointer in ["/process", "/process/process", "/process/exit"] {
        assert_accepts_nested_addition::<ProcessExitedEvent>(exited.clone(), pointer);
    }
    for pointer in [
        "/target",
        "/source_region",
        "/source_region/rect",
        "/source_size",
        "/size",
        "/cursor",
        "/delivery",
    ] {
        assert_accepts_nested_addition::<ScreenshotResult>(screenshot_result(), pointer);
    }
    for pointer in [
        "/evidence",
        "/evidence/transfer",
        "/evidence/terminal",
        "/content",
    ] {
        assert_accepts_nested_addition::<ClipboardReadResult>(clipboard_read_result(), pointer);
    }
    let binary_clipboard = json!({
        "selection": "clipboard",
        "revision": "1",
        "evidence": {
            "target": "application/octet-stream",
            "transfer": { "mode": "direct" },
            "content_length": 0,
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "owner_changed": false,
            "terminal_chunk_observed": false,
            "terminal": { "status": "completed" }
        },
        "content": {
            "delivery": "inline_binary",
            "data": {
                "base64": "",
                "decoded_length": 0,
                "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            }
        }
    });
    assert_accepts_nested_addition::<ClipboardReadResult>(binary_clipboard, "/content/data");

    let page = json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "snapshot_revision": "7",
        "windows": [window_entry()],
        "next_cursor": null
    });
    for pointer in [
        "/windows/0",
        "/windows/0/snapshot",
        "/windows/0/snapshot/ref",
        "/windows/0/snapshot/metadata",
        "/windows/0/snapshot/process",
        "/windows/0/snapshot/state",
    ] {
        assert_accepts_nested_addition::<WindowListPage>(page.clone(), pointer);
    }

    let welcome = json!({
        "type": "server.welcome",
        "protocol": { "major": 1, "minor": 0 },
        "connection_id": "018f1e74-7a6b-7cc0-8000-000000000008",
        "principal": { "id": "viewer-principal", "capabilities": [] },
        "desktop": { "id": desktop(), "generation": generation(), "state": "ready" },
        "limits": {
            "max_message_bytes": 1048576,
            "heartbeat_ms": 15000,
            "normal_outbound_capacity": 128,
            "reserved_outbound_capacity": 16,
            "max_command_watches": 32
        },
        "resume": { "status": "not_requested" }
    });
    for pointer in ["/protocol", "/principal", "/desktop", "/limits", "/resume"] {
        assert_accepts_nested_addition::<WebSocketServerMessage>(welcome.clone(), pointer);
    }

    let page = element_page();
    for pointer in [
        "/elements/0",
        "/elements/0/snapshot",
        "/elements/0/snapshot/ref",
        "/elements/0/snapshot/ref/application",
        "/elements/0/snapshot/role",
        "/elements/0/snapshot/actions/0",
        "/elements/0/snapshot/component",
        "/elements/0/snapshot/component/extents",
        "/elements/0/snapshot/window_correlation",
    ] {
        assert_accepts_nested_addition::<ElementListPage>(page.clone(), pointer);
    }

    let event = json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "atspi_generation": "1",
        "source": element_reference(),
        "raw_source": {
            "bus_name": ":1.42",
            "object_path": "/org/a11y/atspi/accessible/42"
        },
        "kind": "state_changed",
        "resync_reason": null,
        "detail": {
            "property": null,
            "state": "focused",
            "enabled": true,
            "child": null,
            "text": null,
            "value": null,
            "bounds": null
        },
        "revision": "4",
        "cache_sequence": "8",
        "source_stale": false
    });
    assert_accepts_nested_addition::<AccessibilityEvent>(event, "/raw_source");
}

#[test]
fn shared_request_objects_reject_nested_additions() {
    let mut lease = json!({
        "protocol_version": { "major": 1, "minor": 0 },
        "request_id": request(),
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "ttl_ms": 1000
    });
    lease["protocol_version"]["future"] = json!(true);
    assert!(serde_json::from_value::<LeaseAcquireRequest>(lease).is_err());

    let mut screenshot = json!({
        "target": { "kind": "root" },
        "region": { "x": 0, "y": 0, "width": 1, "height": 1 },
        "format": "png",
        "include_cursor": false,
        "scale": null,
        "max_bytes": 1024
    });
    screenshot["target"]["future"] = json!(true);
    assert!(serde_json::from_value::<ScreenshotRequest>(screenshot.clone()).is_err());
    screenshot["target"] = json!({ "kind": "root" });
    screenshot["region"]["future"] = json!(true);
    assert!(serde_json::from_value::<ScreenshotRequest>(screenshot).is_err());

    let mut snapshot_lookup = json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "target": { "type": "reference", "window": window_reference() }
    });
    snapshot_lookup["target"]["window"]["future"] = json!(true);
    assert!(serde_json::from_value::<WindowSnapshotRequest>(snapshot_lookup).is_err());

    let mut artifact_source = json!({
        "source": "artifact",
        "artifact": artifact_reference(),
        "target": "application/octet-stream"
    });
    artifact_source["artifact"]["future"] = json!(true);
    assert!(serde_json::from_value::<ClipboardWriteSource>(artifact_source).is_err());

    let mut text_source = json!({
        "source": "artifact",
        "artifact": artifact_reference()
    });
    text_source["artifact"]["future"] = json!(true);
    assert!(serde_json::from_value::<TextSource>(text_source).is_err());

    let mut binary_source = json!({
        "source": "inline_binary",
        "target": "application/octet-stream",
        "data": {
            "base64": "",
            "decoded_length": 0,
            "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "future": true
        }
    });
    assert!(serde_json::from_value::<ClipboardWriteSource>(binary_source.take()).is_err());

    let mut geometry_command = json!({
        "window": window_reference(),
        "relative_to": "client",
        "geometry": { "x": 10, "y": null, "width": null, "height": null },
        "bounds_policy": "require_inside_root"
    });
    geometry_command["geometry"]["future"] = json!(true);
    assert!(serde_json::from_value::<WindowMoveResizeCommand>(geometry_command).is_err());

    let mut predicate = json!({
        "type": "contains_point",
        "area": "client",
        "point": { "x": 1, "y": 2, "future": true }
    });
    assert!(serde_json::from_value::<WindowGeometryPredicate>(predicate.take()).is_err());
}

#[test]
fn future_event_topics_are_preserved_but_unknown_closed_outputs_fail_loud()
-> Result<(), serde_json::Error> {
    let topic: EventTopic = serde_json::from_value(json!("vendor.future_event"))?;
    assert_eq!(topic.as_str(), "vendor.future_event");

    assert!(
        serde_json::from_value::<WebSocketServerMessage>(json!({
            "type": "server.future_terminal",
            "request_id": request()
        }))
        .is_err()
    );
    let mut result = accepted_command_result();
    result["lifecycle"] = json!("future_terminal");
    assert!(serde_json::from_value::<CommandResult>(result).is_err());
    Ok(())
}

#[test]
fn request_roots_and_malformed_response_values_remain_strict() {
    assert_rejects_additive_field::<CommandEnvelope>(command_envelope());
    assert_rejects_additive_field::<LeaseAcquireRequest>(json!({
        "protocol_version": { "major": 1, "minor": 0 },
        "request_id": request(),
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "ttl_ms": 1000
    }));
    assert_rejects_additive_field::<LeaseRenewRequest>(json!({
        "protocol_version": { "major": 1, "minor": 0 },
        "request_id": request(),
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "lease_id": "018f1e74-7a6b-7cc0-8000-000000000007",
        "ttl_ms": 1000
    }));
    assert_rejects_additive_field::<LeaseReleaseRequest>(json!({
        "protocol_version": { "major": 1, "minor": 0 },
        "request_id": request(),
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "lease_id": "018f1e74-7a6b-7cc0-8000-000000000007"
    }));
    assert_rejects_additive_field::<ClientHello>(json!({
        "type": "client.hello",
        "request_id": request(),
        "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
        "client": { "name": "compat-test", "version": "1.0" },
        "resume": null
    }));
    assert_rejects_additive_field::<WebSocketClientMessage>(json!({
        "type": "client.ping",
        "request_id": request(),
        "nonce": "n"
    }));
    assert_rejects_additive_field::<ScreenshotRequest>(json!({
        "target": { "kind": "root" },
        "region": null,
        "format": "png",
        "include_cursor": false,
        "scale": null,
        "max_bytes": 1024
    }));
    assert_rejects_additive_field::<ClipboardReadRequest>(json!({
        "selection": "clipboard",
        "preferred_targets": [],
        "allow_binary_fallback": false
    }));
    assert_rejects_additive_field::<WindowListRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "limit": 50,
        "order": "xid_ascending",
        "cursor": null
    }));
    assert_rejects_additive_field::<WindowSnapshotRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "target": { "type": "token", "token": "A_window_reference_1" }
    }));
    assert_rejects_additive_field::<WindowQueryRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "selector": selector(),
        "order": "xid_ascending",
        "limit": 50,
        "cursor": null
    }));
    assert_rejects_additive_field::<WindowResolveRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "selector": selector(),
        "order": "xid_ascending",
        "match_policy": "exactly_one"
    }));
    assert_rejects_additive_field::<WindowWaitRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "target": {
            "type": "selector",
            "selector": selector(),
            "quantifier": "any"
        },
        "predicate": { "type": "exists" },
        "after_revision": null,
        "timeout_ms": 1000
    }));
    assert_rejects_additive_field::<ViewerTicketRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "mode": "view_only"
    }));
    assert_rejects_additive_field::<ElementListRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "scope": { "type": "desktop" },
        "order": "preorder",
        "limit": 100,
        "cursor": null,
        "expansion": element_expansion(),
        "limits": accessibility_limits()
    }));
    assert_rejects_additive_field::<ElementQueryRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "selector": element_selector(),
        "limit": 100,
        "cursor": null,
        "expansion": element_expansion(),
        "limits": accessibility_limits()
    }));
    assert_rejects_additive_field::<ElementSnapshotRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "element": element_reference(),
        "expansion": element_expansion()
    }));
    assert_rejects_additive_field::<ElementResolveRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "selector": element_selector(),
        "expansion": element_expansion(),
        "limits": accessibility_limits()
    }));
    assert_rejects_additive_field::<ElementWaitRequest>(json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "target": { "type": "reference", "element": element_reference() },
        "predicate": { "type": "exists" },
        "after_revision": null,
        "timeout_ms": 1000,
        "allow_poll_fallback": true,
        "expansion": element_expansion(),
        "limits": accessibility_limits()
    }));
    assert_rejects_additive_field::<TextInsertCommand>(json!({
        "text": { "source": "inline", "text": "secret" },
        "target": { "target": "window", "window": window_reference() },
        "strategy": "auto",
        "clipboard_options": {
            "preserve_clipboard": false,
            "paste_observation_timeout_ms": 1000
        },
        "semantic_options": null,
        "auto_policy": null
    }));

    let malformed = json!({
        "desktop_id": desktop(),
        "desktop_generation": generation(),
        "state": { "not": "a string" },
        "lease_id": null,
        "expires_at": null,
        "future_additive_field": true
    });
    assert!(serde_json::from_value::<LeaseStateView>(malformed).is_err());
}
