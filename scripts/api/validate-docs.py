#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Dependency-free structural validation for public API documentation."""

from __future__ import annotations

import json
import re
import sys
import uuid
from pathlib import Path
from typing import Any, Iterable
from urllib.parse import unquote


REPO_ROOT = Path(__file__).resolve().parents[2]
API_ROOT = REPO_ROOT / "docs" / "api" / "v1"
OPENAPI_PATH = API_ROOT / "openapi.json"
EXAMPLES_ROOT = API_ROOT / "examples"

IMPLEMENTED_BASE_PATHS = {
    "/livez",
    "/readyz",
    "/v1/status",
    "/v1/capabilities",
    "/v1/desktops/{desktop_id}/lease",
    "/v1/desktops/{desktop_id}/lease/{lease_id}/renew",
    "/v1/desktops/{desktop_id}/lease/{lease_id}",
    "/v1/desktops/{desktop_id}/commands",
    "/v1/desktops/{desktop_id}/commands/{command_id}",
    "/v1/desktops/{desktop_id}/commands/{command_id}/wait",
    "/v1/ws",
}

CURRENT_ROUTE_SOURCES = (
    "observation.rs",
    "accessibility.rs",
    "clipboard_read.rs",
    "screenshot.rs",
    "artifacts.rs",
    "viewer.rs",
    "viewer_gateway.rs",
)


def implemented_current_routes() -> dict[str, set[str]]:
    """Extract literal Axum paths and methods from current public route modules."""
    server_root = REPO_ROOT / "crates" / "xenoteer-server" / "src"
    routes: dict[str, set[str]] = {}
    for source_name in CURRENT_ROUTE_SOURCES:
        source = server_root / source_name
        try:
            text = source.read_text(encoding="utf-8")
        except OSError as error:
            raise ValidationError(f"cannot read route source {source}: {error}") from error
        text = text.partition("\n#[cfg(test)]")[0]
        for match in re.finditer(r'\.route\(\s*"([^"]+)"', text):
            # Axum's catch-all marker is not part of the OpenAPI path-template
            # parameter name.
            route = match.group(1).replace("{*asset}", "{asset}")
            opening = text.find("(", match.start(), match.end())
            depth = 0
            closing = None
            for index in range(opening, len(text)):
                if text[index] == "(":
                    depth += 1
                elif text[index] == ")":
                    depth -= 1
                    if depth == 0:
                        closing = index + 1
                        break
            if closing is None:
                raise ValidationError(f"unterminated route call in {source}: {route}")
            methods = set(
                re.findall(r"(?<!:)\b(get|post|delete)\s*\(", text[opening:closing])
            )
            if not methods:
                raise ValidationError(f"route has no recognized method in {source}: {route}")
            routes.setdefault(route, set()).update(methods)
    return routes

EXPECTED_CURRENT_WS_TYPES = {
    "client.hello",
    "client.ping",
    "server.welcome",
    "server.pong",
    "command.submit",
    "command.watch",
    "command.unwatch",
    "command.cancel",
    "command.accepted",
    "command.progress",
    "command.result",
    "command.unwatched",
    "lease.get",
    "lease.acquire",
    "lease.renew",
    "lease.release",
    "lease.state",
    "events.subscribe",
    "events.unsubscribe",
    "events.subscribed",
    "events.unsubscribed",
    "event",
    "events.replay_complete",
    "events.resync_required",
    "server.draining",
    "error",
}


class ValidationError(Exception):
    """One deterministic documentation-contract failure."""


def load_json(path: Path) -> Any:
    try:
        raw = path.read_bytes()
        if len(raw) > 1_048_576:
            raise ValidationError(f"JSON document exceeds 1 MiB: {path}")
        return json.loads(raw)
    except (OSError, json.JSONDecodeError) as error:
        raise ValidationError(f"cannot parse JSON {path}: {error}") from error


def iter_nodes(value: Any) -> Iterable[Any]:
    yield value
    if isinstance(value, dict):
        for child in value.values():
            yield from iter_nodes(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_nodes(child)


def resolve_pointer(document: Any, pointer: str, source: Path) -> Any:
    if pointer in ("", "#"):
        return document
    if not pointer.startswith("#/"):
        raise ValidationError(f"unsupported JSON pointer in {source}: {pointer}")
    current = document
    for encoded in pointer[2:].split("/"):
        part = unquote(encoded).replace("~1", "/").replace("~0", "~")
        try:
            current = current[int(part)] if isinstance(current, list) else current[part]
        except (KeyError, IndexError, ValueError, TypeError) as error:
            raise ValidationError(f"broken JSON pointer {pointer} in {source}") from error
    return current


def validate_references(document: Any, source: Path) -> set[Path]:
    external_examples: set[Path] = set()
    for node in iter_nodes(document):
        if not isinstance(node, dict):
            continue
        reference = node.get("$ref")
        if isinstance(reference, str):
            file_part, separator, fragment = reference.partition("#")
            if file_part:
                target_path = (source.parent / file_part).resolve()
                if not target_path.is_file():
                    raise ValidationError(f"missing $ref target from {source}: {reference}")
                target = load_json(target_path)
                if separator:
                    resolve_pointer(target, f"#{fragment}", target_path)
            else:
                resolve_pointer(document, f"#{fragment}" if separator else reference, source)
        external_value = node.get("externalValue")
        if isinstance(external_value, str):
            if "://" in external_value:
                raise ValidationError(
                    f"examples must remain checked-in local files in {source}: {external_value}"
                )
            target_path = (source.parent / external_value).resolve()
            if not target_path.is_file():
                raise ValidationError(
                    f"missing externalValue target from {source}: {external_value}"
                )
            load_json(target_path)
            external_examples.add(target_path)
    return external_examples


def validate_openapi(document: Any) -> set[Path]:
    if not isinstance(document, dict) or document.get("openapi") != "3.1.0":
        raise ValidationError("openapi.json must declare OpenAPI 3.1.0")
    info = document.get("info", {})
    if info.get("license", {}).get("identifier") != "Apache-2.0":
        raise ValidationError("public OpenAPI license must be Apache-2.0")
    paths = document.get("paths")
    current_routes = implemented_current_routes()
    implemented_paths = IMPLEMENTED_BASE_PATHS | set(current_routes)
    documented_paths = set(paths) if isinstance(paths, dict) else set()
    if documented_paths != implemented_paths:
        missing = sorted(implemented_paths - documented_paths)
        extra = sorted(documented_paths - implemented_paths)
        raise ValidationError(
            f"OpenAPI path set differs from implemented routes; missing={missing}, extra={extra}"
        )
    for path, implemented_methods in current_routes.items():
        documented_methods = {
            method for method in ("get", "post", "delete") if method in paths[path]
        }
        if documented_methods != implemented_methods:
            raise ValidationError(
                f"OpenAPI method set differs for {path}; "
                f"implemented={sorted(implemented_methods)}, "
                f"documented={sorted(documented_methods)}"
            )
    if document.get("security") != [{"bearerAuth": []}]:
        raise ValidationError("OpenAPI must require Bearer auth globally")
    for public_path in ("/livez", "/readyz"):
        if paths[public_path]["get"].get("security") != []:
            raise ValidationError(f"{public_path} must explicitly override global auth")
    public_viewer_paths = {
        "/viewer/{desktop_id}/{desktop_generation}/",
        "/viewer/assets/viewer.css",
        "/viewer/assets/viewer.mjs",
        "/viewer/vendor/{asset}",
    }
    for public_path in public_viewer_paths:
        if paths[public_path]["get"].get("security") != []:
            raise ValidationError(f"{public_path} must explicitly override global auth")
    viewer_gateway = paths[
        "/v1/desktops/{desktop_id}/generations/{desktop_generation}/viewer/ws"
    ]["get"]
    if viewer_gateway.get("security") != [{"viewerTicketSubprotocol": []}]:
        raise ValidationError("viewer gateway must declare only one-time ticket auth")

    operation_ids: list[str] = []
    for path_item in paths.values():
        for method in ("get", "post", "delete"):
            operation = path_item.get(method)
            if isinstance(operation, dict):
                operation_id = operation.get("operationId")
                if not isinstance(operation_id, str) or not operation_id:
                    raise ValidationError(f"{method} operation is missing operationId")
                operation_ids.append(operation_id)
    if len(operation_ids) != len(set(operation_ids)):
        raise ValidationError("OpenAPI operationId values must be unique")
    return validate_references(document, OPENAPI_PATH)


def validate_uuid_text(value: Any, label: str, source: Path) -> None:
    if value is None:
        return
    if not isinstance(value, str):
        raise ValidationError(f"{label} must be textual in {source}")
    try:
        parsed = uuid.UUID(value)
    except ValueError as error:
        raise ValidationError(f"invalid {label} in {source}: {value}") from error
    if parsed.int == 0:
        raise ValidationError(f"nil {label} in {source}")
    if label == "command_id" and parsed.version not in (4, 7):
        raise ValidationError(f"command_id must use UUIDv4 or UUIDv7 in {source}")


def validate_example(value: Any, source: Path) -> None:
    for node in iter_nodes(value):
        if not isinstance(node, dict):
            continue
        for label in (
            "request_id",
            "command_id",
            "desktop_id",
            "desktop_generation",
            "lease_id",
            "connection_id",
            "launch_id",
        ):
            if label in node:
                validate_uuid_text(node[label], label, source)
        if "code" in node and "status" in node and "detail" in node:
            if not isinstance(node.get("details"), dict):
                raise ValidationError(f"Problem example lacks object details in {source}")

    if isinstance(value, dict) and value.get("type") == "command.submit":
        command = value.get("command", {})
        if value.get("request_id") != command.get("request_id"):
            raise ValidationError(f"outer/inner command request_id mismatch in {source}")
    if isinstance(value, dict) and value.get("type") in {
        "lease.acquire",
        "lease.renew",
        "lease.release",
    }:
        lease = value.get("lease", {})
        if value.get("request_id") != lease.get("request_id"):
            raise ValidationError(f"outer/inner lease request_id mismatch in {source}")

    if isinstance(value, dict) and value.get("type") == "client.hello":
        resume = value.get("resume")
        if resume is not None and (
            not isinstance(resume, dict)
            or not isinstance(resume.get("desktop_generation"), str)
        ):
            raise ValidationError(f"hello resume is not generation-fenced in {source}")
    if isinstance(value, dict) and value.get("type") == "events.subscribe":
        topics = value.get("topics")
        if (
            not isinstance(topics, list)
            or len(topics) > 32
            or any(not isinstance(topic, str) or not topic for topic in topics)
        ):
            raise ValidationError(f"invalid event topics in {source}")
        if len(topics) != len(set(topics)):
            raise ValidationError(f"duplicate event topics in {source}")
        since = value.get("since_sequence")
        if since is not None and (not isinstance(since, int) or since < 0):
            raise ValidationError(f"invalid event replay cursor in {source}")
    if isinstance(value, dict) and value.get("type") == "event":
        event = value.get("event")
        if not isinstance(event, dict) or not isinstance(event.get("sequence"), int):
            raise ValidationError(f"event example lacks a sequence in {source}")
        allowed_topics = {
            "command.lifecycle",
            "action.lifecycle",
            "process.exited",
            "accessibility.element_created",
            "accessibility.element_changed",
            "accessibility.element_removed",
            "accessibility.resync_required",
        }
        if event["sequence"] <= 0 or event.get("topic") not in allowed_topics:
            raise ValidationError(f"event example uses an invalid sequence/topic in {source}")
        payload = event.get("payload")
        if event.get("topic") in {"command.lifecycle", "action.lifecycle"}:
            required_payload = {
                "command_id",
                "command_lifecycle",
                "action_state",
                "updated_monotonic_ms",
                "terminal",
            }
            if not isinstance(payload, dict) or set(payload) != required_payload:
                raise ValidationError(
                    f"event example differs from lifecycle payload in {source}"
                )
        elif event.get("topic") == "process.exited":
            validate_process_exited_payload(payload, source)
        else:
            validate_accessibility_event_payload(payload, source)
    if isinstance(value, dict) and value.get("type") == "events.resync_required":
        reasons = {
            "generation_changed",
            "history_lost",
            "sequence_ahead",
            "subscriber_lag",
            "outbound_backpressure",
        }
        if value.get("reason") not in reasons:
            raise ValidationError(f"invalid event resync reason in {source}")
    if isinstance(value, dict) and value.get("type") == "server.draining":
        if "request_id" in value:
            raise ValidationError(f"server.draining must not claim request correlation in {source}")


def validate_process_exited_payload(payload: Any, source: Path) -> None:
    required_payload = {
        "application",
        "process",
        "termination_requested",
        "forced_escalation",
    }
    if not isinstance(payload, dict) or set(payload) != required_payload:
        raise ValidationError(f"process.exited payload has an invalid shape in {source}")
    if not isinstance(payload["application"], str) or not payload["application"]:
        raise ValidationError(f"process.exited application is invalid in {source}")
    if not isinstance(payload["termination_requested"], bool) or not isinstance(
        payload["forced_escalation"], bool
    ):
        raise ValidationError(f"process.exited cleanup flags are invalid in {source}")
    if payload["forced_escalation"] and not payload["termination_requested"]:
        raise ValidationError(f"process.exited cleanup flags are inconsistent in {source}")

    process = payload["process"]
    if not isinstance(process, dict) or set(process) != {"process", "state", "exit"}:
        raise ValidationError(f"process.exited process view is invalid in {source}")
    if process["state"] != "exited":
        raise ValidationError(f"process.exited state is not terminal in {source}")
    reference = process["process"]
    required_reference = {
        "desktop_generation",
        "pid",
        "proc_start_ticks",
        "launch_id",
    }
    if not isinstance(reference, dict) or set(reference) != required_reference:
        raise ValidationError(f"process.exited reference is invalid in {source}")
    if not isinstance(reference["pid"], int) or reference["pid"] <= 0:
        raise ValidationError(f"process.exited PID is invalid in {source}")
    if not isinstance(reference["proc_start_ticks"], int) or reference["proc_start_ticks"] <= 0:
        raise ValidationError(f"process.exited start ticks are invalid in {source}")

    exit_status = process["exit"]
    if not isinstance(exit_status, dict) or set(exit_status) != {
        "code",
        "signal",
        "core_dumped",
    }:
        raise ValidationError(f"process.exited status is invalid in {source}")
    code = exit_status["code"]
    signal = exit_status["signal"]
    if (code is None) == (signal is None):
        raise ValidationError(f"process.exited status is ambiguous in {source}")
    if code is not None and not isinstance(code, int):
        raise ValidationError(f"process.exited code is invalid in {source}")
    if signal is not None and (
        not isinstance(signal, int) or isinstance(signal, bool) or not 1 <= signal <= 255
    ):
        raise ValidationError(f"process.exited signal is invalid in {source}")
    if not isinstance(exit_status["core_dumped"], bool):
        raise ValidationError(f"process.exited core flag is invalid in {source}")

    forbidden = {"principal", "principal_id", "requester", "stdout", "stderr"}
    if any(isinstance(node, dict) and forbidden.intersection(node) for node in iter_nodes(payload)):
        raise ValidationError(f"process.exited payload discloses private metadata in {source}")


def validate_accessibility_event_payload(payload: Any, source: Path) -> None:
    required = {
        "desktop_id",
        "desktop_generation",
        "atspi_generation",
        "source",
        "kind",
        "detail",
        "revision",
        "cache_sequence",
        "source_stale",
    }
    if not isinstance(payload, dict) or not required.issubset(payload):
        raise ValidationError(f"accessibility event payload has an invalid shape in {source}")
    if not isinstance(payload["atspi_generation"], int) or payload["atspi_generation"] <= 0:
        raise ValidationError(f"accessibility event generation is invalid in {source}")
    if not isinstance(payload["revision"], int) or payload["revision"] <= 0:
        raise ValidationError(f"accessibility event revision is invalid in {source}")
    if not isinstance(payload["cache_sequence"], int) or payload["cache_sequence"] <= 0:
        raise ValidationError(f"accessibility event cache sequence is invalid in {source}")
    detail = payload["detail"]
    if not isinstance(detail, dict):
        raise ValidationError(f"accessibility event detail is invalid in {source}")
    text = detail.get("text")
    if isinstance(text, dict) and text.get("redacted") is True and text.get("content") is not None:
        raise ValidationError(f"redacted accessibility event exposes text in {source}")
    if payload["kind"] == "resync_required":
        if (
            payload["source"] is not None
            or payload.get("raw_source") is not None
            or not isinstance(payload.get("resync_reason"), str)
            or payload["source_stale"] is not False
        ):
            raise ValidationError(f"accessibility resync event has a source in {source}")
    else:
        if not isinstance(payload.get("raw_source"), dict):
            raise ValidationError(f"accessibility event lacks a raw source in {source}")
        if (payload["source"] is None) != (payload["source_stale"] is True):
            raise ValidationError(f"accessibility event source freshness is inconsistent in {source}")
    forbidden = {"password", "secret", "token", "text_content"}
    if any(isinstance(node, dict) and forbidden.intersection(node) for node in iter_nodes(payload)):
        raise ValidationError(f"accessibility event discloses protected metadata in {source}")


def validate_accessibility_request_contract() -> None:
    """Keep reserved metadata hydration surfaces out of the Phase 5 wire schema."""
    query_schema = load_json(REPO_ROOT / "schemas" / "v1" / "element-query-request.json")
    wait_schema = load_json(REPO_ROOT / "schemas" / "v1" / "element-wait-request.json")
    snapshot_schema = load_json(
        REPO_ROOT / "schemas" / "v1" / "element-snapshot-request.json"
    )

    try:
        predicate_variants = {
            variant["properties"]["type"]["const"]
            for variant in query_schema["$defs"]["ElementPredicate"]["oneOf"]
        }
        wait_variants = {
            variant["properties"]["type"]["const"]
            for variant in wait_schema["$defs"]["ElementWaitPredicate"]["oneOf"]
        }
        expansion_fields = set(
            snapshot_schema["$defs"]["ElementSnapshotExpansion"]["properties"]
        )
        component = next(
            variant
            for variant in query_schema["$defs"]["ElementPredicate"]["oneOf"]
            if variant["properties"]["type"]["const"] == "component_intersects"
        )
        geometry = next(
            variant
            for variant in wait_schema["$defs"]["ElementWaitPredicate"]["oneOf"]
            if variant["properties"]["type"]["const"] == "geometry"
        )
        component_spaces = component["properties"]["coordinate_space"]["enum"]
        geometry_spaces = geometry["properties"]["coordinate_space"]["enum"]
    except (KeyError, StopIteration, TypeError) as error:
        raise ValidationError("accessibility request schemas have an unexpected shape") from error

    expected_predicates = {
        "role",
        "name",
        "description",
        "state",
        "interface",
        "value_range",
        "index_in_parent",
        "child_count",
        "component_intersects",
    }
    expected_waits = {
        "exists",
        "gone",
        "state",
        "name",
        "value",
        "child_count",
        "geometry",
        "selector_count",
    }
    if predicate_variants != expected_predicates:
        raise ValidationError("Phase 5 accessibility selector schema exposes reserved metadata")
    if wait_variants != expected_waits:
        raise ValidationError("Phase 5 accessibility wait schema exposes reserved text content")
    if expansion_fields != {"value", "text_metadata", "component"}:
        raise ValidationError("Phase 5 accessibility expansion schema exposes reserved metadata")
    if component_spaces != ["atspi_screen"] or geometry_spaces != ["atspi_screen"]:
        raise ValidationError("Phase 5 accessibility geometry is not limited to AT-SPI screen")


def main() -> int:
    if not (API_ROOT / "README.md").is_file() or not (API_ROOT / "websocket.md").is_file():
        raise ValidationError("versioned API overview or WebSocket contract is missing")
    public_license = API_ROOT.parent / "LICENSE"
    public_notice = API_ROOT.parent / "NOTICE"
    if not public_license.is_file() or not public_notice.is_file():
        raise ValidationError("public API Apache-2.0 LICENSE or NOTICE is missing")
    if public_license.read_bytes() != (REPO_ROOT / "schemas" / "LICENSE").read_bytes():
        raise ValidationError("public API Apache-2.0 license differs from schema boundary")

    policy = (REPO_ROOT / "container" / "licenses" / "first-party-paths.tsv").read_text()
    docs_rule = "docs/api/*\tApache-2.0\tdocs/api/LICENSE|docs/api/NOTICE"
    script_rule = "scripts/api/*\tApache-2.0\tdocs/api/LICENSE|docs/api/NOTICE"
    if docs_rule not in policy or script_rule not in policy:
        raise ValidationError("public API source-inventory rules are missing")
    if policy.index(docs_rule) > policy.index("docs/*\tBUSL-1.1\tLICENSE"):
        raise ValidationError("docs/api Apache rule must precede the broad docs rule")
    if policy.index(script_rule) > policy.index("scripts/*\tBUSL-1.1\tLICENSE"):
        raise ValidationError("scripts/api Apache rule must precede the broad scripts rule")

    document = load_json(OPENAPI_PATH)
    linked_rest_examples = validate_openapi(document)
    validate_accessibility_request_contract()

    examples = sorted(EXAMPLES_ROOT.rglob("*.json"))
    if not examples:
        raise ValidationError("no API examples found")
    current_ws_types: set[str] = set()
    for path in examples:
        value = load_json(path)
        validate_example(value, path)
        if path.parent.name == "ws" and path.name.startswith("current-"):
            message_type = value.get("type") if isinstance(value, dict) else None
            if not isinstance(message_type, str):
                raise ValidationError(f"current WebSocket example lacks type: {path}")
            current_ws_types.add(message_type)

    if current_ws_types != EXPECTED_CURRENT_WS_TYPES:
        missing = sorted(EXPECTED_CURRENT_WS_TYPES - current_ws_types)
        extra = sorted(current_ws_types - EXPECTED_CURRENT_WS_TYPES)
        raise ValidationError(
            f"current WebSocket example inventory differs; missing={missing}, extra={extra}"
        )

    unlinked_rest = {
        path.resolve()
        for path in examples
        if path.parent == EXAMPLES_ROOT
    } - linked_rest_examples
    if unlinked_rest:
        names = ", ".join(sorted(path.name for path in unlinked_rest))
        raise ValidationError(f"REST examples not linked from OpenAPI: {names}")

    print(
        f"validated OpenAPI, {len(examples)} JSON examples, local references, "
        f"and {len(current_ws_types)} implemented WebSocket message types"
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ValidationError as error:
        print(f"API documentation validation failed: {error}", file=sys.stderr)
        sys.exit(1)
