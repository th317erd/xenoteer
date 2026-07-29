#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate Xenoteer's language-neutral v1 conformance corpus.

This module intentionally uses only the Python standard library. SDK packages
may import ``load_corpus`` while their own test adapters remain language
specific.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import sys
from dataclasses import dataclass
from typing import Any, Iterable


FORMAT_VERSION = 1
LICENSE = "Apache-2.0"
CASE_ID = re.compile(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
UUID = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-"
    r"[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)
UINT64_MAX = (1 << 64) - 1
REQUIRED_SUITES = {
    "compatibility.forward",
    "events.continuity",
    "protocol.negotiation",
    "redaction.surfaces",
    "scenarios.command-reconnect",
    "scenarios.effect-stages",
    "scenarios.stale-references",
    "wire.uint64-string",
}
REQUIRED_OPERATIONS = {
    "admit_request_version",
    "classify_terminal_effect",
    "command_reconnect",
    "decode_event",
    "decode_request",
    "decode_response",
    "decode_uint64_string",
    "event_continuity",
    "negotiate_protocol_range",
    "redaction",
    "reference_lifecycle",
}
SCENARIO_OPERATIONS = {
    "classify_terminal_effect",
    "command_reconnect",
    "event_continuity",
    "reference_lifecycle",
}
FORWARD_OPERATIONS = {"decode_event", "decode_request", "decode_response"}


class ConformanceError(RuntimeError):
    """A fail-closed corpus or adapter-contract error."""


@dataclass(frozen=True)
class LoadedCorpus:
    """Validated manifest and immutable ordered case collection."""

    root: pathlib.Path
    manifest: dict[str, Any]
    suites: tuple[dict[str, Any], ...]
    cases: tuple[dict[str, Any], ...]


def _object_without_duplicates(
    pairs: list[tuple[str, Any]],
) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ConformanceError(f"duplicate JSON object key: {key}")
        result[key] = value
    return result


def _load_json(path: pathlib.Path) -> Any:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise ConformanceError(f"cannot read {path}: {error}") from error
    if b"\x00" in raw:
        raise ConformanceError(f"NUL byte in JSON file: {path}")
    if not raw.endswith(b"\n") or raw.endswith(b"\n\n"):
        raise ConformanceError(f"JSON file must end with one newline: {path}")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ConformanceError(f"JSON file is not UTF-8: {path}") from error
    try:
        return json.loads(text, object_pairs_hook=_object_without_duplicates)
    except (json.JSONDecodeError, ConformanceError) as error:
        raise ConformanceError(f"invalid JSON in {path}: {error}") from error


def _require_exact_keys(
    value: dict[str, Any],
    expected: set[str],
    where: str,
) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        raise ConformanceError(
            f"{where} keys differ; missing={missing}, unknown={unknown}"
        )


def _require_int(value: Any, where: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ConformanceError(f"{where} must be an integer")
    if not minimum <= value <= maximum:
        raise ConformanceError(
            f"{where} must be between {minimum} and {maximum}"
        )
    return value


def _sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
    except OSError as error:
        raise ConformanceError(f"cannot hash {path}: {error}") from error
    return digest.hexdigest()


def _corpus_hash(entries: Iterable[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for path, file_hash in entries:
        digest.update(path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def _safe_case_path(root: pathlib.Path, relative: str) -> pathlib.Path:
    if not relative.startswith("cases/"):
        raise ConformanceError(f"suite path is outside cases/: {relative}")
    path = pathlib.PurePosixPath(relative)
    if path.is_absolute() or ".." in path.parts or path.suffix != ".json":
        raise ConformanceError(f"unsafe suite path: {relative}")
    absolute = root.joinpath(*path.parts)
    if absolute.is_symlink() or not absolute.is_file():
        raise ConformanceError(f"suite is not a regular file: {relative}")
    if absolute.resolve().parent != (root / "cases").resolve():
        raise ConformanceError(f"suite escaped the v1 corpus root: {relative}")
    return absolute


def _validate_range(value: Any, where: str) -> tuple[int, int, int]:
    if not isinstance(value, dict):
        raise ConformanceError(f"{where} must be an object")
    _require_exact_keys(value, {"major", "min_minor", "max_minor"}, where)
    major = _require_int(value["major"], f"{where}.major", 0, 65535)
    minimum = _require_int(
        value["min_minor"], f"{where}.min_minor", 0, 65535
    )
    maximum = _require_int(
        value["max_minor"], f"{where}.max_minor", 0, 65535
    )
    return major, minimum, maximum


def _validate_protocol(value: Any, where: str) -> tuple[int, int]:
    if not isinstance(value, dict):
        raise ConformanceError(f"{where} must be an object")
    _require_exact_keys(value, {"major", "minor"}, where)
    return (
        _require_int(value["major"], f"{where}.major", 0, 65535),
        _require_int(value["minor"], f"{where}.minor", 0, 65535),
    )


def _negotiation_outcome(
    client: tuple[int, int, int],
    server: tuple[int, int, int],
) -> tuple[str, Any]:
    client_major, client_min, client_max = client
    server_major, server_min, server_max = server
    if client_min > client_max or server_min > server_max:
        return "rejected", "reversed_minor_range"
    if client_major != server_major:
        return "rejected", "unsupported_major"
    shared_min = max(client_min, server_min)
    shared_max = min(client_max, server_max)
    if shared_min > shared_max:
        return "rejected", "no_shared_minor"
    return "accepted", {"major": client_major, "minor": shared_max}


def _canonical_uint64(value: Any, allow_zero: bool) -> int | None:
    if not isinstance(value, str):
        return None
    if not value or len(value) > 20 or not value.isascii() or not value.isdigit():
        return None
    if len(value) > 1 and value.startswith("0"):
        return None
    parsed = int(value, 10)
    if parsed > UINT64_MAX or (parsed == 0 and not allow_zero):
        return None
    return parsed


def _resolve_pointer(value: Any, pointer: str) -> Any:
    if pointer == "":
        return value
    if not pointer.startswith("/"):
        raise ConformanceError(f"invalid JSON Pointer: {pointer}")
    current = value
    for raw_part in pointer[1:].split("/"):
        part = raw_part.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict) and part in current:
            current = current[part]
        elif isinstance(current, list) and part.isdigit():
            index = int(part, 10)
            if index >= len(current):
                raise ConformanceError(f"JSON Pointer index is absent: {pointer}")
            current = current[index]
        else:
            raise ConformanceError(f"JSON Pointer is absent: {pointer}")
    return current


def _all_strings(value: Any) -> Iterable[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, nested in value.items():
            yield key
            yield from _all_strings(nested)
    elif isinstance(value, list):
        for nested in value:
            yield from _all_strings(nested)


def _validate_negotiation(case: dict[str, Any], where: str) -> None:
    value = case["input"]
    expect = case["expect"]
    if case["operation"] == "negotiate_protocol_range":
        if not isinstance(value, dict):
            raise ConformanceError(f"{where}.input must be an object")
        _require_exact_keys(value, {"client", "server"}, f"{where}.input")
        actual = _negotiation_outcome(
            _validate_range(value["client"], f"{where}.input.client"),
            _validate_range(value["server"], f"{where}.input.server"),
        )
        if expect.get("outcome") != actual[0]:
            raise ConformanceError(f"{where} has an incorrect outcome")
        expected_value = (
            expect.get("selected")
            if actual[0] == "accepted"
            else expect.get("code")
        )
        if expected_value != actual[1]:
            raise ConformanceError(f"{where} has an incorrect negotiation result")
        return

    if not isinstance(value, dict):
        raise ConformanceError(f"{where}.input must be an object")
    _require_exact_keys(
        value,
        {"negotiated", "request"},
        f"{where}.input",
    )
    negotiated = _validate_protocol(
        value["negotiated"], f"{where}.input.negotiated"
    )
    request = _validate_protocol(value["request"], f"{where}.input.request")
    accepted = negotiated == request
    expected = expect.get("outcome") == "accepted"
    if accepted != expected:
        raise ConformanceError(f"{where} has an incorrect request-version outcome")
    if not accepted and expect.get("code") != "unsupported_version":
        raise ConformanceError(f"{where} must reject with unsupported_version")


def _validate_uint64(case: dict[str, Any], where: str) -> None:
    value = case["input"]
    if not isinstance(value, dict):
        raise ConformanceError(f"{where}.input must be an object")
    _require_exact_keys(value, {"allow_zero", "wire"}, f"{where}.input")
    if not isinstance(value["allow_zero"], bool):
        raise ConformanceError(f"{where}.input.allow_zero must be boolean")
    parsed = _canonical_uint64(value["wire"], value["allow_zero"])
    outcome = case["expect"].get("outcome")
    if (parsed is not None) != (outcome == "accepted"):
        raise ConformanceError(f"{where} has an incorrect uint64 outcome")
    if parsed is not None and case["expect"].get("decimal") != str(parsed):
        raise ConformanceError(f"{where} has an incorrect canonical decimal")
    if parsed is None and case["expect"].get("code") != "invalid_uint64_string":
        raise ConformanceError(f"{where} must use invalid_uint64_string")


def _validate_forward(case: dict[str, Any], where: str) -> None:
    value = case["input"]
    if not isinstance(value, dict) or "wire" not in value:
        raise ConformanceError(f"{where}.input.wire is required")
    wire = value["wire"]
    if not isinstance(wire, dict):
        raise ConformanceError(f"{where}.input.wire must be an object")
    preserve = case["expect"].get("preserve", [])
    if not isinstance(preserve, list) or not all(
        isinstance(pointer, str) for pointer in preserve
    ):
        raise ConformanceError(f"{where}.expect.preserve must be pointers")
    for pointer in preserve:
        _resolve_pointer(wire, pointer)
    if case["operation"] == "decode_request":
        if case["expect"].get("outcome") != "rejected":
            raise ConformanceError(f"{where} request unknown fields must reject")
        if case["expect"].get("connection") not in {None, "usable"}:
            raise ConformanceError(f"{where} has an invalid connection outcome")
    elif case["operation"] == "decode_response":
        if case["expect"].get("outcome") not in {
            "accepted",
            "operation_error",
            "unknown_message",
        }:
            raise ConformanceError(f"{where} has an invalid response outcome")
    elif case["expect"].get("outcome") != "unknown_event":
        raise ConformanceError(f"{where} unknown events must be preserved")


def _validate_scenario(case: dict[str, Any], where: str) -> None:
    value = case["input"]
    if not isinstance(value, dict):
        raise ConformanceError(f"{where}.input must be an object")
    expect = case["expect"]
    operation = case["operation"]
    if operation == "command_reconnect":
        _require_exact_keys(
            value,
            {
                "cancel_after_ms",
                "command",
                "command_id",
                "desktop_generation",
                "desktop_id",
                "initial_response",
                "lookup_response",
                "reconnect_generation",
                "resubmit_command",
                "resubmit_response",
            },
            f"{where}.input",
        )
        _require_exact_keys(
            expect,
            {
                "cancel_requests",
                "command_id",
                "error_code",
                "lifecycle",
                "lookup_attempts",
                "outcome",
                "submission_envelopes",
                "submission_attempts",
            },
            f"{where}.expect",
        )
        if not isinstance(value["command"], dict):
            raise ConformanceError(f"{where}.input.command must be an object")
        for field in ("initial_response", "lookup_response", "resubmit_response"):
            response = value[field]
            if response is not None:
                _validate_transport_response(response, f"{where}.input.{field}")
        if value["resubmit_command"] is not None and not isinstance(
            value["resubmit_command"], dict
        ):
            raise ConformanceError(
                f"{where}.input.resubmit_command must be object or null"
            )
        cancel_after = value["cancel_after_ms"]
        if cancel_after is not None:
            _require_int(cancel_after, f"{where}.input.cancel_after_ms", 1, 60_000)
        for field in ("submission_attempts", "lookup_attempts", "cancel_requests"):
            _require_int(expect[field], f"{where}.expect.{field}", 0, 16)
        envelopes = expect["submission_envelopes"]
        commands = [value["command"]]
        if value["resubmit_command"] is not None:
            commands.append(value["resubmit_command"])
        if not isinstance(envelopes, list) or len(envelopes) != len(commands):
            raise ConformanceError(
                f"{where}.expect.submission_envelopes must cover every submission"
            )
        if expect["submission_attempts"] != len(envelopes):
            raise ConformanceError(
                f"{where}.expect.submission_attempts must match submission_envelopes"
            )
        for index, (envelope, command) in enumerate(zip(envelopes, commands)):
            envelope_where = f"{where}.expect.submission_envelopes[{index}]"
            if not isinstance(envelope, dict):
                raise ConformanceError(f"{envelope_where} must be an object")
            _require_exact_keys(
                envelope,
                {
                    "command",
                    "command_id",
                    "deadline",
                    "desktop_generation",
                    "desktop_id",
                    "lease_id",
                    "protocol_version",
                    "request_id_non_nil",
                    "trace_policy",
                },
                envelope_where,
            )
            if envelope["command"] != command:
                raise ConformanceError(
                    f"{envelope_where}.command must equal the submitted command"
                )
            if envelope["command_id"] != value["command_id"]:
                raise ConformanceError(
                    f"{envelope_where}.command_id must equal input.command_id"
                )
            for field in ("desktop_id", "desktop_generation"):
                if envelope[field] != value[field]:
                    raise ConformanceError(
                        f"{envelope_where}.{field} must equal input.{field}"
                    )
            _require_exact_keys(
                envelope["protocol_version"],
                {"major", "minor"},
                f"{envelope_where}.protocol_version",
            )
            if envelope["protocol_version"] != {"major": 1, "minor": 0}:
                raise ConformanceError(
                    f"{envelope_where}.protocol_version must be v1.0"
                )
            if envelope["request_id_non_nil"] is not True:
                raise ConformanceError(
                    f"{envelope_where}.request_id_non_nil must be true"
                )
            for field in ("deadline", "lease_id", "trace_policy"):
                if envelope[field] is not None:
                    raise ConformanceError(
                        f"{envelope_where}.{field} must be null"
                    )
    elif operation == "classify_terminal_effect":
        _require_exact_keys(value, {"result"}, f"{where}.input")
        _require_exact_keys(
            expect,
            {
                "details",
                "effect_stage",
                "error_code",
                "has_visible_effect",
                "lifecycle",
                "outcome_type",
                "retry",
                "warning_count",
            },
            f"{where}.expect",
        )
        if not isinstance(value["result"], dict):
            raise ConformanceError(f"{where}.input.result must be an object")
        if not isinstance(expect["details"], dict):
            raise ConformanceError(f"{where}.expect.details must be an object")
        if not isinstance(expect["has_visible_effect"], bool):
            raise ConformanceError(
                f"{where}.expect.has_visible_effect must be boolean"
            )
        _require_int(expect["warning_count"], f"{where}.expect.warning_count", 0, 64)
    elif operation == "event_continuity":
        _require_exact_keys(
            value,
            {
                "cursor_generation",
                "desktop_generation",
                "desktop_id",
                "frames",
                "initial_cursor",
                "queue_capacity",
                "subscription_request_id",
                "topics",
            },
            f"{where}.input",
        )
        _require_exact_keys(
            expect,
            {
                "delivered_sequences",
                "final_cursor",
                "generation_changed",
                "refresh_required",
                "resync_reason",
                "terminal",
            },
            f"{where}.expect",
        )
        if (
            not isinstance(value["frames"], list)
            or not value["frames"]
            or not all(isinstance(frame, dict) for frame in value["frames"])
        ):
            raise ConformanceError(f"{where}.input.frames must be message objects")
        if not isinstance(value["topics"], list) or not all(
            isinstance(topic, str) and topic for topic in value["topics"]
        ):
            raise ConformanceError(f"{where}.input.topics must be strings")
        for field in (
            "cursor_generation",
            "desktop_generation",
            "desktop_id",
            "subscription_request_id",
        ):
            if (
                not isinstance(value[field], str)
                or not UUID.fullmatch(value[field])
                or value[field] == "00000000-0000-0000-0000-000000000000"
            ):
                raise ConformanceError(
                    f"{where}.input.{field} must be a canonical non-nil UUID"
                )
        initial_cursor = value["initial_cursor"]
        if initial_cursor is not None and _canonical_uint64(
            initial_cursor, True
        ) is None:
            raise ConformanceError(
                f"{where}.input.initial_cursor must be a canonical uint64 or null"
            )
        _require_int(value["queue_capacity"], f"{where}.input.queue_capacity", 1, 4096)
        if not isinstance(expect["delivered_sequences"], list) or not all(
            _canonical_uint64(sequence, False) is not None
            for sequence in expect["delivered_sequences"]
        ):
            raise ConformanceError(
                f"{where}.expect.delivered_sequences must be uint64 strings"
            )
        final_cursor = expect["final_cursor"]
        if final_cursor is not None and _canonical_uint64(
            final_cursor, True
        ) is None:
            raise ConformanceError(
                f"{where}.expect.final_cursor must be a canonical uint64 or null"
            )
        if expect["terminal"] not in {
            None,
            "invalid_message",
            "queue_overflow",
            "resync_required",
        }:
            raise ConformanceError(f"{where}.expect.terminal is unsupported")
        resync_reason = expect["resync_reason"]
        if resync_reason is not None and (
            not isinstance(resync_reason, str)
            or len(resync_reason.encode("utf-8")) > 128
            or not CASE_ID.fullmatch(resync_reason)
        ):
            raise ConformanceError(
                f"{where}.expect.resync_reason must be canonical text or null"
            )
        for field in ("generation_changed", "refresh_required"):
            if not isinstance(expect[field], bool):
                raise ConformanceError(f"{where}.expect.{field} must be boolean")
    elif operation == "reference_lifecycle":
        _require_exact_keys(
            value,
            {"current", "kind", "original", "relocated", "server_problem"},
            f"{where}.input",
        )
        _require_exact_keys(
            expect,
            {
                "generation_changed",
                "identity_unchanged",
                "relocated_distinct",
                "server_error_code",
                "stale",
            },
            f"{where}.expect",
        )
        if value["kind"] not in {"element", "window"}:
            raise ConformanceError(f"{where}.input.kind is unsupported")
        for field in ("current", "original", "server_problem"):
            if not isinstance(value[field], dict):
                raise ConformanceError(f"{where}.input.{field} must be an object")
        if value["relocated"] is not None and not isinstance(value["relocated"], dict):
            raise ConformanceError(
                f"{where}.input.relocated must be object or null"
            )
        for field in (
            "generation_changed",
            "identity_unchanged",
            "relocated_distinct",
            "stale",
        ):
            if not isinstance(expect[field], bool):
                raise ConformanceError(f"{where}.expect.{field} must be boolean")
    else:
        raise ConformanceError(f"{where} has unsupported scenario operation")


def _validate_transport_response(value: Any, where: str) -> None:
    if not isinstance(value, dict) or "kind" not in value:
        raise ConformanceError(f"{where} must be a transport response object")
    kind = value["kind"]
    if kind == "disconnect":
        _require_exact_keys(value, {"kind"}, where)
    elif kind == "stall":
        _require_exact_keys(value, {"delay_ms", "kind"}, where)
        _require_int(value["delay_ms"], f"{where}.delay_ms", 1, 60_000)
    elif kind in {"json", "problem"}:
        _require_exact_keys(value, {"body", "kind", "status"}, where)
        _require_int(value["status"], f"{where}.status", 100, 599)
        if not isinstance(value["body"], dict):
            raise ConformanceError(f"{where}.body must be an object")
    else:
        raise ConformanceError(f"{where}.kind is unsupported")


def _validate_redaction(case: dict[str, Any], where: str) -> None:
    value = case["input"]
    if not isinstance(value, dict):
        raise ConformanceError(f"{where}.input must be an object")
    _require_exact_keys(
        value,
        {"base_url", "kind", "raw", "secret"},
        f"{where}.input",
    )
    _require_exact_keys(
        case["expect"],
        {"debug_leaked", "error_leaked", "url_leaked"},
        f"{where}.expect",
    )
    if value["kind"] not in {"artifact", "bearer", "clipboard", "command", "viewer"}:
        raise ConformanceError(f"{where}.input.kind is unsupported")
    if not isinstance(value["secret"], str) or not value["secret"]:
        raise ConformanceError(f"{where}.input.secret must be nonempty text")
    if not isinstance(value["raw"], dict):
        raise ConformanceError(f"{where}.input.raw must be an object")
    if not isinstance(value["base_url"], str):
        raise ConformanceError(f"{where}.input.base_url must be text")
    if not any(
        value["secret"] in candidate for candidate in _all_strings(value["raw"])
    ):
        raise ConformanceError(f"{where}.input.raw must contain the secret")
    for field in ("debug_leaked", "error_leaked", "url_leaked"):
        if not isinstance(case["expect"][field], bool):
            raise ConformanceError(f"{where}.expect.{field} must be boolean")


def _validate_case(case: Any, suite: str, index: int) -> dict[str, Any]:
    where = f"{suite}.cases[{index}]"
    if not isinstance(case, dict):
        raise ConformanceError(f"{where} must be an object")
    required = {"description", "expect", "id", "input", "operation", "tags"}
    _require_exact_keys(case, required, where)
    case_id = case["id"]
    if not isinstance(case_id, str) or not CASE_ID.fullmatch(case_id):
        raise ConformanceError(f"{where}.id is not canonical")
    if not isinstance(case["description"], str) or not case["description"]:
        raise ConformanceError(f"{where}.description must be nonempty")
    tags = case["tags"]
    if (
        not isinstance(tags, list)
        or not tags
        or not all(isinstance(tag, str) and CASE_ID.fullmatch(tag) for tag in tags)
        or len(tags) != len(set(tags))
    ):
        raise ConformanceError(f"{where}.tags must be unique canonical strings")
    if not isinstance(case["expect"], dict) or not case["expect"]:
        raise ConformanceError(f"{where}.expect must be a nonempty object")
    operation = case["operation"]
    if not isinstance(operation, str) or operation not in REQUIRED_OPERATIONS:
        raise ConformanceError(f"{where}.operation is unsupported: {operation}")
    if operation in {"admit_request_version", "negotiate_protocol_range"}:
        _validate_negotiation(case, where)
    elif operation == "decode_uint64_string":
        _validate_uint64(case, where)
    elif operation in FORWARD_OPERATIONS:
        _validate_forward(case, where)
    elif operation in SCENARIO_OPERATIONS:
        _validate_scenario(case, where)
    elif operation == "redaction":
        _validate_redaction(case, where)
    return case


def _validate_suite(value: Any, expected_name: str) -> tuple[dict[str, Any], ...]:
    if not isinstance(value, dict):
        raise ConformanceError(f"suite {expected_name} must be an object")
    _require_exact_keys(
        value,
        {"cases", "description", "format_version", "license", "suite"},
        f"suite {expected_name}",
    )
    if value["format_version"] != FORMAT_VERSION:
        raise ConformanceError(f"suite {expected_name} format version differs")
    if value["license"] != LICENSE:
        raise ConformanceError(f"suite {expected_name} is not Apache-2.0")
    if value["suite"] != expected_name:
        raise ConformanceError(f"suite name differs from manifest: {expected_name}")
    if not isinstance(value["description"], str) or not value["description"]:
        raise ConformanceError(f"suite {expected_name} description is empty")
    cases = value["cases"]
    if not isinstance(cases, list) or not cases:
        raise ConformanceError(f"suite {expected_name} has no cases")
    validated = tuple(
        _validate_case(case, expected_name, index)
        for index, case in enumerate(cases)
    )
    case_ids = [case["id"] for case in validated]
    if case_ids != sorted(case_ids) or len(case_ids) != len(set(case_ids)):
        raise ConformanceError(
            f"suite {expected_name} case IDs must be unique and sorted"
        )
    return validated


def load_corpus(root: pathlib.Path | None = None) -> LoadedCorpus:
    """Load and fully validate a corpus root."""

    if root is None:
        root = pathlib.Path(__file__).resolve().parents[2] / "conformance" / "v1"
    root = root.resolve()
    manifest_path = root / "manifest.json"
    manifest = _load_json(manifest_path)
    if not isinstance(manifest, dict):
        raise ConformanceError("manifest must be an object")
    _require_exact_keys(
        manifest,
        {
            "corpus",
            "corpus_sha256",
            "format_version",
            "license",
            "protocol",
            "suites",
        },
        "manifest",
    )
    if manifest["corpus"] != "xenoteer-conformance-v1":
        raise ConformanceError("unexpected corpus identifier")
    if manifest["format_version"] != FORMAT_VERSION:
        raise ConformanceError("unsupported corpus format version")
    if manifest["license"] != LICENSE:
        raise ConformanceError("corpus is not Apache-2.0")
    protocol = manifest["protocol"]
    if not isinstance(protocol, dict):
        raise ConformanceError("manifest protocol must be an object")
    _require_exact_keys(
        protocol,
        {"major", "max_minor", "min_minor"},
        "manifest.protocol",
    )
    major = _require_int(protocol["major"], "protocol.major", 0, 65535)
    minimum = _require_int(
        protocol["min_minor"], "protocol.min_minor", 0, 65535
    )
    maximum = _require_int(
        protocol["max_minor"], "protocol.max_minor", 0, 65535
    )
    if major != 1 or minimum > maximum:
        raise ConformanceError("manifest protocol range is invalid")

    entries = manifest["suites"]
    if not isinstance(entries, list) or not entries:
        raise ConformanceError("manifest suites must be a nonempty array")
    paths: list[str] = []
    names: list[str] = []
    hashes: list[tuple[str, str]] = []
    suites: list[dict[str, Any]] = []
    cases: list[dict[str, Any]] = []
    for index, entry in enumerate(entries):
        where = f"manifest.suites[{index}]"
        if not isinstance(entry, dict):
            raise ConformanceError(f"{where} must be an object")
        _require_exact_keys(
            entry,
            {"case_count", "path", "sha256", "suite"},
            where,
        )
        path_value = entry["path"]
        name = entry["suite"]
        file_hash = entry["sha256"]
        if (
            not isinstance(path_value, str)
            or not isinstance(name, str)
            or not CASE_ID.fullmatch(name)
            or not isinstance(file_hash, str)
            or not SHA256.fullmatch(file_hash)
        ):
            raise ConformanceError(f"{where} contains a malformed value")
        _require_int(entry["case_count"], f"{where}.case_count", 1, 10000)
        path = _safe_case_path(root, path_value)
        actual_hash = _sha256(path)
        if actual_hash != file_hash:
            raise ConformanceError(
                f"{path_value} hash differs: expected {file_hash}, got {actual_hash}"
            )
        suite_value = _load_json(path)
        suite_cases = _validate_suite(suite_value, name)
        if entry["case_count"] != len(suite_cases):
            raise ConformanceError(f"{where}.case_count differs from suite")
        paths.append(path_value)
        names.append(name)
        hashes.append((path_value, file_hash))
        suites.append(suite_value)
        cases.extend(suite_cases)

    if paths != sorted(paths) or len(paths) != len(set(paths)):
        raise ConformanceError("manifest suite paths must be unique and sorted")
    if len(names) != len(set(names)):
        raise ConformanceError("manifest suite names must be unique")
    if set(names) != REQUIRED_SUITES:
        raise ConformanceError(
            f"manifest suite coverage differs: {sorted(set(names))}"
        )
    actual_case_files = sorted(
        path.relative_to(root).as_posix()
        for path in (root / "cases").glob("*.json")
        if path.is_file()
    )
    if actual_case_files != paths:
        raise ConformanceError(
            "manifest does not exactly cover every regular cases/*.json file"
        )
    expected_corpus_hash = manifest["corpus_sha256"]
    if (
        not isinstance(expected_corpus_hash, str)
        or not SHA256.fullmatch(expected_corpus_hash)
        or _corpus_hash(hashes) != expected_corpus_hash
    ):
        raise ConformanceError("manifest corpus_sha256 differs")

    case_ids = [case["id"] for case in cases]
    if len(case_ids) != len(set(case_ids)):
        raise ConformanceError("case IDs must be globally unique")
    operations = {case["operation"] for case in cases}
    if operations != REQUIRED_OPERATIONS:
        raise ConformanceError(
            f"operation coverage differs: {sorted(operations)}"
        )
    return LoadedCorpus(
        root=root,
        manifest=manifest,
        suites=tuple(suites),
        cases=tuple(cases),
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        help="alternate conformance/v1 root (used by package tests)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit a stable machine-readable summary",
    )
    arguments = parser.parse_args(argv)
    try:
        corpus = load_corpus(arguments.root)
    except ConformanceError as error:
        print(f"conformance validation failed: {error}", file=sys.stderr)
        return 1
    summary = {
        "cases": len(corpus.cases),
        "corpus": corpus.manifest["corpus"],
        "corpus_sha256": corpus.manifest["corpus_sha256"],
        "suites": len(corpus.suites),
    }
    if arguments.json:
        print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
    else:
        print(
            "validated Xenoteer v1 conformance corpus: "
            f"{summary['suites']} suites, {summary['cases']} cases, "
            f"sha256 {summary['corpus_sha256']}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
