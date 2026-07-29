# SPDX-License-Identifier: Apache-2.0
"""Concrete adapter for the checked-in Xenoteer v1 conformance corpus."""

from __future__ import annotations

import asyncio
import copy
import hashlib
import json
import uuid
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

import httpx

from .client import (
    ProtocolVersion,
    admit_request_version,
    negotiate_protocol,
    validate_status,
)
from .command import (
    CommandSubmission,
    classify_terminal_effect,
    validate_client_command_envelope,
    validate_command_result,
)
from .desktop import Desktop, Element, ViewerTicket, Window
from .errors import XenoteerError
from .events import (
    EventSession,
    KnownEvent,
    ReplayComplete,
    ResyncRequired,
    UnknownEvent,
    UnknownServerMessage,
    decode_event_message,
    decode_server_message,
)
from .options import ClientOptions, ProtocolRange
from .state import GenerationRegistry
from .transport import HttpTransport
from .wire import decode_uint64


_TOKEN = "x" * 48
_REFERENCE_TOKEN = "r" * 32
FROZEN_CORPUS = "xenoteer-conformance-v1"
FROZEN_CORPUS_SHA256 = (
    "6cc98e72e1de6591cce2d0661f4fc3ea508535d310a40746aa3ad8bd1e61e7fc"
)
FROZEN_FORMAT_VERSION = 1
FROZEN_LICENSE = "Apache-2.0"
FROZEN_PROTOCOL = {"major": 1, "min_minor": 0, "max_minor": 0}


@dataclass(frozen=True, slots=True)
class ConformanceResult:
    case_id: str
    status: str
    detail: str


def run_v1_conformance(root: Path | None = None) -> list[ConformanceResult]:
    """Verify corpus integrity and execute every concrete fixture."""

    root = (
        Path(__file__).resolve().parents[4] / "conformance" / "v1"
        if root is None
        else Path(root)
    )
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    if (
        not isinstance(manifest, dict)
        or manifest.get("corpus") != FROZEN_CORPUS
        or manifest.get("corpus_sha256") != FROZEN_CORPUS_SHA256
        or manifest.get("format_version") != FROZEN_FORMAT_VERSION
        or manifest.get("license") != FROZEN_LICENSE
        or manifest.get("protocol") != FROZEN_PROTOCOL
        or not isinstance(manifest.get("suites"), list)
    ):
        raise XenoteerError(
            "conformance_integrity", "corpus identity differs from frozen v1"
        )
    suites: list[dict[str, Any]] = manifest["suites"]
    aggregate = hashlib.sha256()
    cases: list[dict[str, Any]] = []
    for suite in sorted(suites, key=lambda value: value["path"]):
        relative = suite["path"]
        data = (root / relative).read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if digest != suite["sha256"]:
            raise XenoteerError("conformance_integrity", f"{relative} digest mismatch")
        document = json.loads(data)
        if (
            document["suite"] != suite["suite"]
            or len(document["cases"]) != suite["case_count"]
        ):
            raise XenoteerError(
                "conformance_integrity", f"{relative} manifest mismatch"
            )
        aggregate.update(relative.encode("utf-8"))
        aggregate.update(b"\0")
        aggregate.update(digest.encode("ascii"))
        aggregate.update(b"\n")
        cases.extend(document["cases"])
    if aggregate.hexdigest() != manifest["corpus_sha256"]:
        raise XenoteerError(
            "conformance_integrity", "corpus aggregate digest mismatch"
        )
    return run_cases(cases)


def run_cases(cases: list[dict[str, Any]]) -> list[ConformanceResult]:
    """Execute runner-supplied fixtures without consulting case IDs."""

    try:
        asyncio.get_running_loop()
    except RuntimeError:
        return asyncio.run(_run_cases(cases))
    raise XenoteerError(
        "invalid_state",
        "synchronous conformance execution cannot run inside an event loop",
    )


async def _run_cases(cases: list[dict[str, Any]]) -> list[ConformanceResult]:
    results: list[ConformanceResult] = []
    for case in cases:
        try:
            await _run_case(case)
        except Exception as error:
            results.append(
                ConformanceResult(
                    str(case.get("id", "<missing>")),
                    "failed",
                    f"{type(error).__name__}: {error}",
                )
            )
        else:
            results.append(
                ConformanceResult(
                    str(case.get("id", "<missing>")),
                    "passed",
                    "concrete SDK behavior exercised",
                )
            )
    return results


async def _run_case(case: dict[str, Any]) -> None:
    operation = case.get("operation")
    if operation == "decode_uint64_string":
        _uint64_case(case)
    elif operation == "negotiate_protocol_range":
        _negotiation_case(case)
    elif operation == "admit_request_version":
        _request_version_case(case)
    elif operation == "classify_terminal_effect":
        _effect_case(case)
    elif operation in {"decode_event", "decode_request", "decode_response"}:
        _forward_case(case)
    elif operation == "redaction":
        await _redaction_case(case)
    elif operation == "command_reconnect":
        await _command_reconnect_case(case)
    elif operation == "event_continuity":
        await _event_continuity_case(case)
    elif operation == "reference_lifecycle":
        await _reference_lifecycle_case(case)
    else:
        raise AssertionError(f"unknown corpus operation {operation!r}")


def _uint64_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "uint64 input")
    try:
        value = decode_uint64(input_.get("wire"), allow_zero=input_["allow_zero"])
        actual = {"outcome": "accepted", "decimal": str(value)}
    except (TypeError, ValueError):
        actual = {"outcome": "rejected", "code": "invalid_uint64_string"}
    _require_expected(actual, case)


def _negotiation_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "negotiation input")
    client = _object(input_.get("client"), "client range")
    server = _object(input_.get("server"), "server range")
    try:
        selected = negotiate_protocol(
            ProtocolRange(client["major"], client["min_minor"], client["max_minor"]),
            ProtocolVersion(server["major"], server["min_minor"]),
            ProtocolVersion(server["major"], server["max_minor"]),
        )
        actual: dict[str, Any] = {
            "outcome": "accepted",
            "selected": selected.wire(),
        }
    except XenoteerError as error:
        actual = {"outcome": "rejected", "code": error.code}
    _require_expected(actual, case)


def _request_version_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "request version input")
    negotiated = ProtocolVersion(**_object(input_.get("negotiated"), "negotiated"))
    request = ProtocolVersion(**_object(input_.get("request"), "request"))
    try:
        admit_request_version(negotiated, request)
        actual = {"outcome": "accepted"}
    except XenoteerError as error:
        actual = {"outcome": "rejected", "code": error.code}
    _require_expected(actual, case)


def _effect_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "effect input")
    result = _object(input_.get("result"), "command result")
    command_id = result.get("command_id")
    if not isinstance(command_id, str):
        raise AssertionError("command result ID is absent")
    validated = validate_command_result(result, command_id)
    classified = classify_terminal_effect(validated)
    error = validated.get("error")
    error_wire = error if isinstance(error, Mapping) else {}
    outcome = validated.get("outcome")
    outcome_type = outcome.get("type") if isinstance(outcome, Mapping) else None
    actual = {
        "lifecycle": validated["lifecycle"],
        "effect_stage": classified.effect_stage,
        "has_visible_effect": classified.visible_effect,
        "error_code": error_wire.get("code"),
        "retry": error_wire.get("retry"),
        "details": copy.deepcopy(error_wire.get("details", {})),
        "warning_count": len(validated["warnings"]),
        "outcome_type": outcome_type,
    }
    _require_expected(actual, case)


def _forward_case(case: dict[str, Any]) -> None:
    operation = case["operation"]
    input_ = _object(case.get("input"), "forward input")
    wire = _object(input_.get("wire"), "forward wire")
    preserved: object = wire
    if operation == "decode_event":
        event = decode_event_message(wire)
        if not isinstance(event, UnknownEvent):
            raise AssertionError("future event topic was not preserved")
        actual = {
            "outcome": "unknown_event",
            "preserve": [
                "/event/payload",
                "/event/sequence",
                "/event/topic",
            ],
        }
        preserved = event.raw
    elif operation == "decode_request":
        try:
            validate_client_command_envelope(wire)
        except XenoteerError as error:
            actual = {
                "outcome": "rejected",
                "code": error.code,
                "connection": "usable",
                "preserve": [],
            }
        else:
            actual = {
                "outcome": "accepted",
                "connection": "usable",
                "preserve": [],
            }
    elif "server_version" in wire:
        try:
            status = validate_status(wire)
        except XenoteerError as error:
            actual = {
                "outcome": "operation_error",
                "code": error.code,
                "connection": "usable",
                "preserve": [],
            }
        else:
            actual = {
                "outcome": "accepted",
                "known_type": "status",
                "preserve": _status_additive_pointers(wire),
            }
            preserved = status.raw
    elif wire.get("type") == "command.result":
        result = _object(wire.get("result"), "command result")
        command_id = result.get("command_id")
        try:
            validate_command_result(result, cast(str, command_id))
        except XenoteerError as error:
            actual = {
                "outcome": "operation_error",
                "code": error.code,
                "connection": "usable",
                "preserve": ["/result/outcome"],
            }
        else:
            actual = {
                "outcome": "accepted",
                "connection": "usable",
                "preserve": [],
            }
    else:
        decoded = decode_server_message(wire)
        if not isinstance(decoded, UnknownServerMessage):
            raise AssertionError("future server message was not preserved")
        actual = {
            "outcome": "unknown_message",
            "connection": "usable",
            "preserve": [""],
        }
        preserved = decoded.raw
    preserve_pointers = actual.get("preserve")
    if not isinstance(preserve_pointers, list) or any(
        not isinstance(pointer, str) for pointer in preserve_pointers
    ):
        raise AssertionError("implementation preservation proof is malformed")
    for pointer in preserve_pointers:
        if _pointer(preserved, pointer) != _pointer(wire, pointer):
            raise AssertionError(f"preserved JSON pointer changed: {pointer}")
    _require_expected(actual, case, unordered_fields={"preserve"})


class _CommandHttpFixture:
    def __init__(self, input_: dict[str, Any]) -> None:
        self._input = input_
        self.submission_bodies: list[bytes] = []
        self.lookup_attempts = 0
        self.cancel_requests = 0
        self.stall_started = asyncio.Event()
        self._stall_release = asyncio.Event()

    async def __call__(self, request: httpx.Request) -> httpx.Response:
        if request.method == "POST" and request.url.path.endswith("/commands"):
            self.submission_bodies.append(await request.aread())
            index = len(self.submission_bodies) - 1
            fixture = (
                self._input["initial_response"]
                if index == 0
                else self._input["resubmit_response"]
            )
            return await self._response(request, fixture)
        if request.method == "GET" and "/commands/" in request.url.path:
            self.lookup_attempts += 1
            return await self._response(request, self._input["lookup_response"])
        if request.method == "DELETE" and "/commands/" in request.url.path:
            self.cancel_requests += 1
            raise AssertionError("fixture did not declare a cancellation response")
        raise AssertionError(f"unexpected fixture request {request.method} {request.url.path}")

    async def _response(
        self, request: httpx.Request, value: object
    ) -> httpx.Response:
        fixture = _object(value, "transport response")
        kind = fixture.get("kind")
        if kind == "disconnect":
            raise httpx.ReadError("fixture disconnected", request=request)
        if kind == "stall":
            self.stall_started.set()
            await self._stall_release.wait()
            raise httpx.ReadTimeout("fixture stall elapsed", request=request)
        if kind not in {"json", "problem"}:
            raise AssertionError(f"unsupported transport fixture {kind!r}")
        content_type = (
            "application/problem+json" if kind == "problem" else "application/json"
        )
        return httpx.Response(
            fixture["status"],
            headers={"content-type": content_type},
            content=json.dumps(
                fixture["body"],
                ensure_ascii=False,
                allow_nan=False,
                separators=(",", ":"),
                sort_keys=True,
            ).encode("utf-8"),
            request=request,
        )


async def _command_reconnect_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "command reconnect input")
    fixture = _CommandHttpFixture(input_)
    submissions: list[CommandSubmission] = []
    handle = None
    failure: XenoteerError | None = None
    async with httpx.AsyncClient(
        base_url="https://xenoteer.invalid",
        transport=httpx.MockTransport(fixture),
    ) as http:
        transport = HttpTransport(
            ClientOptions("https://xenoteer.invalid", _TOKEN),
            http_client=http,
        )
        desktop = Desktop(
            transport,
            cast(str, input_["desktop_id"]),
            cast(str, input_["desktop_generation"]),
            {"major": 1, "minor": 0},
        )
        first = desktop.submit(
            _object(input_.get("command"), "command"),
            command_id=cast(str, input_["command_id"]),
        )
        submissions.append(first)
        if _object(input_["initial_response"], "initial response").get("kind") == "stall":
            send_task = asyncio.create_task(first.send())
            await asyncio.wait_for(fixture.stall_started.wait(), timeout=1)
            send_task.cancel()
            try:
                await send_task
            except asyncio.CancelledError:
                pass
            else:
                raise AssertionError("local command send did not cancel")
        else:
            try:
                handle = await first.send()
            except XenoteerError as error:
                failure = error

        same_generation = (
            input_["reconnect_generation"] == input_["desktop_generation"]
        )
        outcome: str
        if not same_generation:
            desktop.registry.observe(cast(str, input_["reconnect_generation"]))
            if not desktop.registry.stale:
                raise AssertionError("generation change did not fence the desktop")
            transport_attempts = len(fixture.submission_bodies)
            try:
                await first.send()
            except XenoteerError as error:
                if error.code != "generation_changed":
                    raise
                failure = error
            else:
                raise AssertionError("stale command submission was not fenced")
            if len(fixture.submission_bodies) != transport_attempts:
                raise AssertionError("generation fence allowed command transport")
            outcome = "stale_generation"
        else:
            lookup_fixture = input_.get("lookup_response")
            if lookup_fixture is not None:
                try:
                    handle = await desktop.command(cast(str, input_["command_id"]))
                    failure = None
                    outcome = "reattached"
                except XenoteerError as error:
                    failure = error
                    outcome = error.problem_code or error.code
            else:
                outcome = failure.problem_code or failure.code if failure else "submitted"

            resubmit_command = input_.get("resubmit_command")
            if resubmit_command is not None and (
                lookup_fixture is None
                or (
                    failure is not None
                    and (failure.problem_code or failure.code) == "not_found"
                )
            ):
                second = desktop.submit(
                    _object(resubmit_command, "resubmit command"),
                    command_id=cast(str, input_["command_id"]),
                )
                submissions.append(second)
                try:
                    handle = await second.send()
                    failure = None
                    outcome = "resubmitted"
                except XenoteerError as error:
                    failure = error
                    outcome = error.problem_code or error.code

        if len(fixture.submission_bodies) != len(submissions):
            raise AssertionError("submission transport count differed")
        for raw, submission in zip(fixture.submission_bodies, submissions):
            if raw != submission.canonical_body:
                raise AssertionError("wire body differed from retained canonical body")
        envelopes = [_normalized_envelope(value.envelope) for value in submissions]
        actual = {
            "outcome": outcome,
            "command_id": input_["command_id"],
            "submission_attempts": len(submissions),
            "lookup_attempts": fixture.lookup_attempts,
            "cancel_requests": fixture.cancel_requests,
            "submission_envelopes": envelopes,
            "lifecycle": None if handle is None else handle.latest["lifecycle"],
            "error_code": (
                None
                if failure is None
                else failure.problem_code or failure.code
            ),
        }
        _require_expected(actual, case)


def _normalized_envelope(value: Mapping[str, Any]) -> dict[str, Any]:
    envelope = copy.deepcopy(dict(value))
    request_id = envelope.pop("request_id", None)
    try:
        parsed = uuid.UUID(cast(str, request_id))
    except (TypeError, ValueError, AttributeError):
        request_id_non_nil = False
    else:
        request_id_non_nil = parsed.int != 0
    return {
        "protocol_version": envelope.get("protocol_version"),
        "request_id_non_nil": request_id_non_nil,
        "command_id": envelope.get("command_id"),
        "desktop_id": envelope.get("desktop_id"),
        "desktop_generation": envelope.get("desktop_generation"),
        "lease_id": envelope.get("lease_id"),
        "deadline": envelope.get("deadline"),
        "trace_policy": envelope.get("trace_policy"),
        "command": envelope.get("command"),
    }


class _EventFixtureSocket:
    def __init__(self, input_: dict[str, Any]) -> None:
        self._input = input_
        self._receives: asyncio.Queue[str] = asyncio.Queue()
        self._receives.put_nowait(json.dumps(_welcome(input_), separators=(",", ":")))
        self._subscription_count = 0
        self._completion_request_id = str(
            uuid.uuid5(
                uuid.UUID(cast(str, input_["subscription_request_id"])),
                "xenoteer-conformance-fixture-complete",
            )
        )
        self._completion_ready = asyncio.Event()
        self._completion_release = asyncio.Event()
        self.closed = False

    async def send(self, encoded: str) -> None:
        message = json.loads(encoded)
        if message.get("type") == "client.hello":
            return
        if message.get("type") != "events.subscribe":
            raise AssertionError("fixture received an unexpected client message")
        initial_subscription = self._subscription_count == 0
        expected_request_id = (
            self._input["subscription_request_id"]
            if initial_subscription
            else self._completion_request_id
        )
        if (
            self._subscription_count >= 2
            or message.get("request_id") != expected_request_id
            or message.get("desktop_id") != self._input["desktop_id"]
            or message.get("desktop_generation") != self._input["desktop_generation"]
            or message.get("topics") != self._input["topics"]
            or (
                initial_subscription
                and message.get("since_sequence") != self._input["initial_cursor"]
            )
        ):
            raise AssertionError("SDK subscription differed from fixture input")
        self._subscription_count += 1
        self._receives.put_nowait(
            json.dumps(
                {
                    "type": "events.subscribed",
                    "request_id": message["request_id"],
                    "topics": message["topics"],
                },
                separators=(",", ":"),
            )
        )
        if initial_subscription:
            for frame in self._input["frames"]:
                self._receives.put_nowait(json.dumps(frame, separators=(",", ":")))

    async def recv(self) -> str:
        encoded = await self._receives.get()
        message = json.loads(encoded)
        if (
            message.get("type") == "events.subscribed"
            and message.get("request_id") == self._completion_request_id
        ):
            self._completion_ready.set()
            await self._completion_release.wait()
        return encoded

    async def wait_for_completion(self) -> None:
        await self._completion_ready.wait()

    def release_completion(self) -> None:
        self._completion_release.set()

    @property
    def completion_request_id(self) -> str:
        return self._completion_request_id

    async def close(self, **kwargs: Any) -> None:
        self.closed = True
        self._completion_release.set()


async def _complete_event_fixture(
    session: EventSession,
    socket: _EventFixtureSocket,
    input_: dict[str, Any],
) -> tuple[str | None, XenoteerError | None]:
    """Fence fixture production after every declared frame reaches the SDK reader."""

    completion = asyncio.create_task(
        session.subscribe(
            cast(str, input_["desktop_id"]),
            cast(str, input_["desktop_generation"]),
            cast(list[str], input_["topics"]),
            since_sequence=session.resume_cursor,
            timeout=1,
            request_id=socket.completion_request_id,
        )
    )
    ready = asyncio.create_task(socket.wait_for_completion())
    final_cursor: str | None = None
    cursor_captured = False
    failure: XenoteerError | None = None
    try:
        done, _ = await asyncio.wait(
            {completion, ready},
            timeout=1,
            return_when=asyncio.FIRST_COMPLETED,
        )
        if not done:
            raise AssertionError("event fixture completion barrier did not resolve")
        if ready in done:
            await ready
            final_cursor = session.resume_cursor
            cursor_captured = True
            socket.release_completion()
        try:
            await completion
        except XenoteerError as error:
            failure = error
        if not cursor_captured:
            final_cursor = session.resume_cursor
        return final_cursor, failure
    finally:
        socket.release_completion()
        if not ready.done():
            ready.cancel()
        if not completion.done():
            completion.cancel()
        for task in (ready, completion):
            try:
                await task
            except (asyncio.CancelledError, XenoteerError):
                pass


async def _event_continuity_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "event continuity input")
    socket = _EventFixtureSocket(input_)

    async def factory(url: str, **kwargs: Any) -> _EventFixtureSocket:
        return socket

    registry = GenerationRegistry(cast(str, input_["cursor_generation"]))
    session = await EventSession.connect(
        "wss://xenoteer.invalid/v1/ws",
        f"Bearer {_TOKEN}",
        _hello(input_),
        capacity=cast(int, input_["queue_capacity"]),
        websocket_factory=factory,
        heartbeat_interval=300,
        read_stale_timeout=600,
        max_reconnect_attempts=0,
        registry=registry,
    )
    delivered: list[str] = []
    terminal: str | None = None
    reason: str | None = None
    completion_failure: XenoteerError | None = None
    stream_failure: XenoteerError | None = None
    final_cursor: str | None = None
    try:
        await session.subscribe(
            cast(str, input_["desktop_id"]),
            cast(str, input_["desktop_generation"]),
            cast(list[str], input_["topics"]),
            since_sequence=cast(str | None, input_["initial_cursor"]),
            timeout=1,
            request_id=cast(str, input_["subscription_request_id"]),
        )
        final_cursor, completion_failure = await _complete_event_fixture(
            session, socket, input_
        )
        await session.close()
        while True:
            try:
                item = await anext(session)
            except StopAsyncIteration:
                break
            except XenoteerError as error:
                stream_failure = error
                if error.code == "generation_changed":
                    terminal = (
                        "resync_required" if reason is not None else "invalid_message"
                    )
                elif error.code == "resync_required":
                    terminal = "resync_required"
                elif error.code == "backpressure":
                    terminal = "queue_overflow"
                elif error.code == "invalid_response":
                    terminal = "invalid_message"
                else:
                    raise
                break
            if isinstance(item, (KnownEvent, UnknownEvent)):
                delivered.append(cast(str, item.raw["event"]["sequence"]))
            elif isinstance(item, ResyncRequired):
                reason = item.reason
            elif isinstance(item, ReplayComplete):
                continue
        if completion_failure is not None:
            if stream_failure is None:
                raise completion_failure
    finally:
        socket.release_completion()
        await session.close()
    if final_cursor is None:
        final_cursor = session.resume_cursor
    actual = {
        "delivered_sequences": delivered,
        "final_cursor": final_cursor,
        "terminal": terminal,
        "resync_reason": reason,
        "refresh_required": terminal
        in {"invalid_message", "resync_required", "queue_overflow"},
        "generation_changed": registry.stale,
    }
    _require_expected(actual, case)


async def _reference_lifecycle_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "reference input")
    original = _object(input_.get("original"), "original reference")
    current = _object(input_.get("current"), "current reference")
    relocated = input_.get("relocated")
    problem = _object(input_.get("server_problem"), "stale problem")
    kind = input_.get("kind")

    async def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/resolve"):
            if relocated is None:
                raise AssertionError("fixture did not declare relocation")
            ref = _object(relocated, "relocated reference")
            if kind == "window":
                body: dict[str, Any] = {
                    "desktop_id": original["desktop_id"],
                    "desktop_generation": original["desktop_generation"],
                    "snapshot_revision": "1",
                    "window": {
                        "snapshot": {"ref": ref, "model_revision": "1"},
                        "reference_token": _REFERENCE_TOKEN,
                    },
                }
            else:
                body = {
                    "desktop_id": original["desktop_id"],
                    "desktop_generation": original["desktop_generation"],
                    "atspi_generation": ref["atspi_generation"],
                    "snapshot_revision": "1",
                    "element": {"snapshot": {"ref": ref, "revision": "1"}},
                }
            return httpx.Response(
                200,
                headers={"content-type": "application/json"},
                json=body,
                request=request,
            )
        return httpx.Response(
            problem["status"],
            headers={"content-type": "application/problem+json"},
            json=problem,
            request=request,
        )

    async with httpx.AsyncClient(
        base_url="https://xenoteer.invalid",
        transport=httpx.MockTransport(handler),
    ) as http:
        desktop = Desktop(
            HttpTransport(
                ClientOptions("https://xenoteer.invalid", _TOKEN),
                http_client=http,
            ),
            cast(str, original["desktop_id"]),
            cast(str, original["desktop_generation"]),
            {"major": 1, "minor": 0},
        )
        old: Window | Element
        current_handle: Window | Element
        if kind == "window":
            old = desktop.windows.handle(
                original, reference_token=_REFERENCE_TOKEN
            )
            current_handle = desktop.windows.handle(current)
        elif kind == "element":
            old = desktop.accessibility.handle(original)
            current_handle = desktop.accessibility.handle(current)
        else:
            raise AssertionError(f"unsupported reference kind {kind!r}")
        original_identity = old.identity
        try:
            await old.snapshot()
        except XenoteerError as error:
            server_error_code = error.problem_code or error.code
        else:
            raise AssertionError("stale server fixture was accepted")
        fresh: Window | Element | None = None
        if relocated is not None:
            fresh = await old.relocate({"type": "all"})
        actual = {
            "stale": old.stale,
            "server_error_code": server_error_code,
            "identity_unchanged": old.identity == original_identity,
            "relocated_distinct": (
                fresh is not None and fresh.identity != old.identity
            ),
            "generation_changed": current_handle.identity != old.identity,
        }
        _require_expected(actual, case)


async def _redaction_case(case: dict[str, Any]) -> None:
    input_ = _object(case.get("input"), "redaction input")
    raw = _object(input_.get("raw"), "redaction raw")
    secret = input_.get("secret")
    if not isinstance(secret, str) or not any(
        secret in value for value in _all_strings(raw)
    ):
        raise AssertionError("raw fixture does not contain its declared secret")
    base_url = cast(str, input_["base_url"])
    observed_urls: list[str] = []

    async def handler(request: httpx.Request) -> httpx.Response:
        observed_urls.append(str(request.url))
        await request.aread()
        raise RuntimeError(secret)

    debug_surfaces: list[str] = []
    error_surfaces: list[str] = []
    async with httpx.AsyncClient(
        base_url=base_url,
        transport=httpx.MockTransport(handler),
    ) as http:
        kind = input_.get("kind")
        token = secret if kind == "bearer" else _TOKEN
        options = ClientOptions(base_url, token)
        transport = HttpTransport(options, http_client=http)
        desktop = Desktop(
            transport,
            "20000000-0000-4000-8000-000000000001",
            "30000000-0000-4000-8000-000000000001",
            {"major": 1, "minor": 0},
        )
        try:
            if kind == "artifact":
                debug_surfaces.append(repr(desktop.artifacts))
                await desktop.artifacts.upload_clipboard_input(
                    cast(str, raw["content_type"]),
                    cast(str, raw["bytes_utf8"]).encode("utf-8"),
                )
            elif kind == "bearer":
                debug_surfaces.extend((repr(options), repr(transport)))
                await transport.request("GET", "/v1/status")
            elif kind in {"clipboard", "command"}:
                submission = desktop.submit(
                    _object(raw.get("command"), "redaction command")
                )
                debug_surfaces.append(repr(submission))
                await submission.send()
            elif kind == "viewer":
                ticket = ViewerTicket(
                    _object(raw.get("ticket"), "viewer ticket")
                )
                debug_surfaces.append(repr(ticket))
                await transport.request("GET", "/v1/status")
            else:
                raise AssertionError(f"unsupported redaction kind {kind!r}")
        except XenoteerError as error:
            error_surfaces.extend((str(error), repr(error)))
        else:
            raise AssertionError("redaction fixture transport unexpectedly succeeded")
    actual = {
        "debug_leaked": any(secret in value for value in debug_surfaces),
        "error_leaked": any(secret in value for value in error_surfaces),
        "url_leaked": any(secret in value for value in observed_urls),
    }
    _require_expected(actual, case)


def _hello(input_: Mapping[str, Any]) -> dict[str, Any]:
    initial_cursor = input_.get("initial_cursor")
    return {
        "type": "client.hello",
        "request_id": str(uuid.uuid4()),
        "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
        "client": {"name": "xenoteer-conformance", "version": "1"},
        "resume": (
            None
            if initial_cursor is None
            else {
                "desktop_id": input_["desktop_id"],
                "desktop_generation": input_["cursor_generation"],
                "event_sequence": initial_cursor,
            }
        ),
    }


def _welcome(input_: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "type": "server.welcome",
        "protocol": {"major": 1, "minor": 0},
        "connection_id": "10000000-0000-4000-8000-000000000001",
        "principal": {"id": "conformance", "capabilities": ["desktop:observe"]},
        "desktop": {
            "id": input_["desktop_id"],
            "generation": input_["desktop_generation"],
            "state": "ready",
        },
        "limits": {
            "max_message_bytes": 1_048_576,
            "heartbeat_ms": 300_000,
            "normal_outbound_capacity": 32,
            "reserved_outbound_capacity": 8,
            "max_command_watches": 16,
        },
        "resume": {"status": "not_requested"},
    }


def _require_expected(
    actual: Mapping[str, Any],
    case: Mapping[str, Any],
    *,
    unordered_fields: set[str] | None = None,
) -> None:
    expected = _object(case.get("expect"), "case expectation")
    normalized_actual = copy.deepcopy(dict(actual))
    normalized_expected = copy.deepcopy(expected)
    for field in unordered_fields or set():
        if field in normalized_actual:
            normalized_actual[field] = sorted(normalized_actual[field])
        if field in normalized_expected:
            normalized_expected[field] = sorted(normalized_expected[field])
    if normalized_actual != normalized_expected:
        raise AssertionError(
            "observed outcome differed: "
            f"actual={json.dumps(normalized_actual, sort_keys=True)}, "
            f"expected={json.dumps(normalized_expected, sort_keys=True)}"
        )


def _pointer(value: object, pointer: str) -> object:
    if pointer == "":
        return value
    if not pointer.startswith("/"):
        raise AssertionError("preserve entry is not a JSON pointer")
    current = value
    for encoded in pointer[1:].split("/"):
        part = encoded.replace("~1", "/").replace("~0", "~")
        if isinstance(current, Mapping):
            if part not in current:
                raise AssertionError(f"JSON pointer is absent: {pointer}")
            current = current[part]
        elif isinstance(current, list) and part.isdigit():
            index = int(part)
            if index >= len(current):
                raise AssertionError(f"JSON pointer is absent: {pointer}")
            current = current[index]
        else:
            raise AssertionError(f"JSON pointer is absent: {pointer}")
    return current


def _status_additive_pointers(wire: Mapping[str, Any]) -> list[str]:
    known: dict[str, set[str]] = {
        "": {
            "server_version",
            "protocol_min",
            "protocol_max",
            "server_time",
            "desktop",
            "capabilities",
        },
        "/protocol_min": {"major", "minor"},
        "/protocol_max": {"major", "minor"},
        "/desktop": {"id", "generation", "state", "reason_code"},
    }
    pointers: list[str] = []
    for parent, recognized in known.items():
        value = _pointer(wire, parent)
        if not isinstance(value, Mapping):
            continue
        for key in value:
            if key not in recognized:
                escaped = str(key).replace("~", "~0").replace("/", "~1")
                pointers.append(f"{parent}/{escaped}")
    return sorted(pointers)


def _object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, Mapping) or not all(
        isinstance(key, str) for key in value
    ):
        raise AssertionError(f"{label} must be an object")
    return copy.deepcopy(dict(value))


def _all_strings(value: object) -> list[str]:
    strings: list[str] = []
    if isinstance(value, str):
        strings.append(value)
    elif isinstance(value, Mapping):
        for key, nested in value.items():
            strings.append(str(key))
            strings.extend(_all_strings(nested))
    elif isinstance(value, list):
        for nested in value:
            strings.extend(_all_strings(nested))
    return strings
