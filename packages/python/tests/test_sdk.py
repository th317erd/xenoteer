# SPDX-License-Identifier: Apache-2.0
"""Focused success and failure-path tests for the public Python SDK."""

from __future__ import annotations

import asyncio
import importlib.util
import io
import json
import pathlib
import tarfile
import tempfile
import tomllib
import unittest
from collections.abc import Callable, Mapping
from typing import Any
from zipfile import ZipFile

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from packaging.version import Version

from xenoteer import (
    ArtifactRef,
    BearerToken,
    ClientOptions,
    CommandHandle,
    ControlLease,
    Desktop,
    ProtocolRange,
    ReconnectPolicy,
    ResyncRequired,
    UINT64_MAX,
    UnknownEvent,
    XenoteerClient,
    XenoteerError,
    decode_event_message,
    decode_uint64,
    encode_uint64,
    validate_status,
)
from xenoteer.transport import request_with_deadline


DESKTOP_ID = "10000000-0000-4000-8000-000000000001"
GENERATION = "10000000-0000-4000-8000-000000000002"
LEASE_ID = "10000000-0000-4000-8000-000000000003"
COMMAND_ID = "10000000-0000-4000-8000-000000000004"
TOKEN = "s" * 48


def status(
    *,
    minimum: tuple[int, int] = (1, 0),
    maximum: tuple[int, int] = (1, 0),
) -> dict[str, Any]:
    return {
        "server_version": "0.1.0",
        "protocol_min": {"major": minimum[0], "minor": minimum[1]},
        "protocol_max": {"major": maximum[0], "minor": maximum[1]},
        "server_time": "2026-07-23T00:00:00Z",
        "desktop": {
            "id": DESKTOP_ID,
            "generation": GENERATION,
            "state": "ready",
            "reason_code": None,
            "future": True,
        },
        "capabilities": {"capabilities": []},
        "future": {"additive": True},
    }


def command_result(command_id: str = COMMAND_ID, lifecycle: str = "accepted") -> dict[str, Any]:
    return {
        "command_id": command_id,
        "lifecycle": lifecycle,
        "effect_stage": "accepted" if lifecycle == "accepted" else "pointer_moved",
        "accepted_at": "2026-07-23T00:00:00Z",
        "warnings": [],
    }


def welcome_message() -> str:
    return json.dumps(
        {
            "type": "server.welcome",
            "protocol": {"major": 1, "minor": 0},
            "connection_id": "10000000-0000-4000-8000-000000000099",
            "principal": {"id": "test", "capabilities": ["desktop:observe"]},
            "desktop": {
                "id": DESKTOP_ID,
                "generation": GENERATION,
                "state": "ready",
            },
            "limits": {
                "max_message_bytes": 1_048_576,
                "heartbeat_ms": 15_000,
                "normal_outbound_capacity": 32,
                "reserved_outbound_capacity": 8,
                "max_command_watches": 16,
            },
            "resume": {"status": "not_requested"},
        }
    )


class FakeTransport:
    def __init__(self, responder: Callable[..., dict[str, Any]]) -> None:
        self.responder = responder
        self.calls: list[tuple[str, str, object, dict[str, str]]] = []
        self.request_timeouts: list[float | None] = []
        self.closed = False

    @property
    def base_url(self) -> str:
        return "https://xenoteer.test"

    async def authorization_header(self) -> str:
        return f"Bearer {TOKEN}"

    async def request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
    ) -> dict[str, Any]:
        self.calls.append((method, path, body, dict(headers or {})))
        self.request_timeouts.append(None)
        return self.responder(method, path, body, dict(headers or {}))

    async def request_with_timeout(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
        timeout: float,
    ) -> dict[str, Any]:
        self.calls.append((method, path, body, dict(headers or {})))
        self.request_timeouts.append(timeout)
        return self.responder(method, path, body, dict(headers or {}))

    async def close(self) -> None:
        self.closed = True


class DeadlineFailureTransport(FakeTransport):
    def __init__(self, failure: BaseException | None = None) -> None:
        super().__init__(lambda *_args: {"ok": True})
        self.failure = failure

    async def request_with_timeout(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
        timeout: float,
    ) -> dict[str, Any]:
        if self.failure is not None:
            raise self.failure
        await asyncio.sleep(timeout * 10)
        return {"ok": True}


class WireTests(unittest.TestCase):
    def test_uint64_boundaries_never_accept_float_or_json_number(self) -> None:
        self.assertEqual(encode_uint64(UINT64_MAX), "18446744073709551615")
        self.assertEqual(decode_uint64("9007199254740993"), 9_007_199_254_740_993)
        for wire in (
            1,
            1.0,
            True,
            "",
            "01",
            "+1",
            "-1",
            " 1",
            "18446744073709551616",
        ):
            with self.subTest(wire=wire), self.assertRaises(TypeError):
                decode_uint64(wire)
        with self.assertRaises(ValueError):
            encode_uint64(1.0)  # type: ignore[arg-type]

    def test_unknown_event_preserves_raw_digits_and_additive_payload(self) -> None:
        message = {
            "type": "event",
            "request_id": COMMAND_ID,
            "event": {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "sequence": "9007199254740993",
                "topic": "future.widget.changed",
                "payload": {"future": [1, 2, 3]},
                "additive": True,
            },
            "top_future": {"x": 1},
        }
        event = decode_event_message(message)
        self.assertIsInstance(event, UnknownEvent)
        self.assertEqual(event.sequence, 9_007_199_254_740_993)
        self.assertEqual(event.raw["event"]["sequence"], "9007199254740993")
        self.assertEqual(event.raw["top_future"], {"x": 1})
        raw = event.raw
        raw["event"]["sequence"] = "1"
        self.assertEqual(event.raw["event"]["sequence"], "9007199254740993")

    def test_event_rejects_numeric_sequence(self) -> None:
        with self.assertRaisesRegex(XenoteerError, "uint64"):
            decode_event_message(
                {
                    "type": "event",
                    "request_id": COMMAND_ID,
                    "event": {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "sequence": 9_007_199_254_740_993,
                        "topic": "future",
                        "payload": {},
                    },
                }
            )


class RedactionTests(unittest.IsolatedAsyncioTestCase):
    async def test_tokens_and_options_are_redacted(self) -> None:
        token = BearerToken(TOKEN)
        options = ClientOptions("https://xenoteer.test", TOKEN)
        self.assertNotIn(TOKEN, repr(token))
        self.assertNotIn(TOKEN, str(token))
        self.assertNotIn(TOKEN, repr(options))
        self.assertEqual(options.safe_dict()["token"], "<redacted>")

        async def broken_provider() -> str:
            raise RuntimeError(f"provider leaked {TOKEN}")

        from xenoteer.options import resolve_token

        with self.assertRaises(XenoteerError) as caught:
            await resolve_token(broken_provider)
        self.assertNotIn(TOKEN, str(caught.exception))
        self.assertNotIn(TOKEN, repr(caught.exception))

    async def test_client_repr_and_viewer_ticket_are_redacted(self) -> None:
        ticket_secret = "a" * 43

        def responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            if path.endswith("/viewer-tickets"):
                return {
                    "ticket": ticket_secret,
                    "principal_id": "tester",
                    "audience": "viewer_websocket",
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "origin": "https://viewer.example",
                    "mode": "view_only",
                    "issued_at": "2026-07-23T00:00:00Z",
                    "expires_at": "2026-07-23T00:00:30Z",
                    "use_policy": "single_use",
                }
            raise AssertionError(path)

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        self.assertNotIn(TOKEN, repr(client))
        ticket = await client.desktop().viewer.ticket("https://viewer.example")
        self.assertNotIn(ticket_secret, repr(ticket))
        self.assertEqual(ticket.expose_ticket(), ticket_secret)
        await client.close()


class ClientTests(unittest.IsolatedAsyncioTestCase):
    async def test_status_rejects_malformed_known_capability_fields(self) -> None:
        valid_capability = {
            "id": "input.pointer.smooth",
            "status": "available",
            "reason_code": None,
            "backend_version": "1.0",
            "future": True,
        }
        accepted = status()
        accepted["capabilities"] = {
            "capabilities": [valid_capability],
            "future": True,
        }
        self.assertEqual(
            validate_status(accepted).capabilities["future"],
            True,
        )

        malformed_reports = (
            {},
            {"capabilities": "not-an-array"},
            {"capabilities": [valid_capability, valid_capability]},
            {"capabilities": [{**valid_capability, "id": "Input.Pointer"}]},
            {"capabilities": [{**valid_capability, "status": "future_status"}]},
            {"capabilities": [{**valid_capability, "reason_code": "NOT_STABLE"}]},
            {"capabilities": [{**valid_capability, "backend_version": "bad\nversion"}]},
            {
                "capabilities": [
                    {
                        **valid_capability,
                        "id": f"capability.{index}",
                    }
                    for index in range(257)
                ]
            },
        )
        for capabilities in malformed_reports:
            candidate = status()
            candidate["capabilities"] = capabilities
            with self.subTest(capabilities=capabilities):
                with self.assertRaisesRegex(XenoteerError, "capabil"):
                    validate_status(candidate)

    async def test_connect_negotiates_highest_common_minor_and_fences_desktop(self) -> None:
        transport = FakeTransport(
            lambda method, path, body, headers: status(minimum=(1, 1), maximum=(1, 3))
        )
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                TOKEN,
                protocol_range=ProtocolRange(1, 0, 2),
            ),
            transport=transport,
            transport_ownership="client",
        )
        self.assertEqual(client.negotiated_protocol.minor, 2)
        desktop = client.desktop()
        self.assertEqual(desktop.id, DESKTOP_ID)
        self.assertEqual(desktop.generation, GENERATION)
        self.assertEqual(client.status.raw["future"], {"additive": True})
        self.assertNotIn(TOKEN, client.deadline_after(1))
        await client.close()
        self.assertTrue(transport.closed)

    async def test_connect_fails_closed_on_no_shared_version(self) -> None:
        transport = FakeTransport(
            lambda method, path, body, headers: status(minimum=(2, 0), maximum=(2, 0))
        )
        with self.assertRaisesRegex(XenoteerError, "overlap"):
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", TOKEN),
                transport=transport,
            )

    async def test_submit_once_and_handle_reads_never_replay(self) -> None:
        seen_id: str | None = None

        def responder(method, path, body, headers):
            nonlocal seen_id
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/commands"):
                seen_id = body["command_id"]
                self.assertEqual(headers["idempotency-key"], seen_id)
                self.assertEqual(
                    body["command"]["process"]["proc_start_ticks"],
                    "9007199254740993",
                )
                return command_result(seen_id)
            if method == "GET" and "/commands/" in path:
                return command_result(seen_id, "succeeded")
            raise AssertionError((method, path))

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        handle = await client.desktop().submit(
            {
                "type": "process_status",
                "process": {
                    "desktop_generation": GENERATION,
                    "pid": 10,
                    "proc_start_ticks": 9_007_199_254_740_993,
                    "launch_id": COMMAND_ID,
                },
            }
        )
        await handle.wait_once(0.001)
        self.assertTrue(handle.terminal)
        submissions = [
            call for call in transport.calls if call[0] == "POST" and call[1].endswith("/commands")
        ]
        self.assertEqual(len(submissions), 1)
        await client.close()

    async def test_submission_exposes_identity_before_io_and_retains_exact_body(self) -> None:
        attempts = 0

        def responder(method, path, body, headers):
            nonlocal attempts
            if path == "/v1/status":
                return status()
            attempts += 1
            if attempts == 1:
                raise XenoteerError("transport", "simulated disconnect")
            return command_result(body["command_id"])

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        submission = client.desktop().submit(
            {"type": "selection_set", "selection": "clipboard", "content": {"secret": TOKEN}}
        )
        self.assertIsNone(submission.envelope["trace_policy"])
        self.assertEqual(
            client.desktop()
            .submit({"type": "desktop_probe"}, trace_policy="detailed")
            .envelope["trace_policy"],
            "detailed",
        )
        retained_id = submission.id
        retained_body = submission.canonical_body
        self.assertNotIn(TOKEN, repr(submission))
        self.assertEqual(len(transport.calls), 1)  # status only; preparation is pre-I/O
        with self.assertRaises(XenoteerError):
            await submission.send()
        self.assertEqual(submission.id, retained_id)
        self.assertEqual(submission.canonical_body, retained_body)
        handle = await submission.send()  # explicit caller decision after simulated lookup
        self.assertEqual(handle.id, retained_id)
        self.assertEqual(submission.canonical_body, retained_body)
        self.assertEqual(attempts, 2)
        await client.close()

    async def test_generation_mismatch_and_numeric_revision_fail_closed(self) -> None:
        responses = [
            {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": "20000000-0000-4000-8000-000000000002",
                "snapshot_revision": "1",
                "windows": [],
                "next_cursor": None,
            },
            {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "snapshot_revision": 9_007_199_254_740_993,
                "windows": [],
                "next_cursor": None,
            },
        ]

        def responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            return responses.pop(0)

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        with self.assertRaisesRegex(XenoteerError, "generation"):
            await client.desktop().windows.query({"type": "all"})
        with self.assertRaisesRegex(XenoteerError, "uint64"):
            await client.desktop().windows.query({"type": "all"})
        await client.close()


class LeaseAndDomainTests(unittest.IsolatedAsyncioTestCase):
    async def test_semantic_waits_use_exact_per_request_deadline_headroom(
        self,
    ) -> None:
        def responder(method, path, body, headers):
            del method, body, headers
            if path.endswith("/accessibility/elements/query"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                }
            if path.endswith("/windows/wait"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                }
            if path.endswith("/accessibility/elements/wait"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                }
            if "/commands/" in path and path.endswith("/wait?timeout_ms=30000"):
                return command_result()
            raise AssertionError(path)

        transport = FakeTransport(responder)
        desktop = Desktop(
            transport,
            DESKTOP_ID,
            GENERATION,
            {"major": 1, "minor": 0},
        )
        await desktop.accessibility.query({})
        await desktop.windows.wait({"timeout_ms": 300_000})
        await desktop.accessibility.wait({"timeout_ms": 120_000})
        command = CommandHandle(
            transport,
            DESKTOP_ID,
            GENERATION,
            command_result(),
        )
        await command.wait_once(30)
        self.assertEqual(transport.request_timeouts, [None, 305.0, 125.0, 35.0])

        call_count = len(transport.calls)
        for invalid in (0, 300_001, True, None):
            with self.subTest(window_timeout=invalid), self.assertRaises(XenoteerError):
                await desktop.windows.wait({"timeout_ms": invalid})
        for invalid in (0, 120_001, True, None):
            with self.subTest(element_timeout=invalid), self.assertRaises(XenoteerError):
                await desktop.accessibility.wait({"timeout_ms": invalid})
        self.assertEqual(len(transport.calls), call_count)

    async def test_legacy_transport_waits_use_local_deadline_without_signature_break(
        self,
    ) -> None:
        class LegacyTransport:
            base_url = "https://xenoteer.test"

            def __init__(self) -> None:
                self.calls: list[str] = []

            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del method, body, headers
                self.calls.append(path)
                if path.endswith("/windows/wait"):
                    return {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                    }
                if path.endswith("/accessibility/elements/wait"):
                    return {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                    }
                if path.endswith("/wait?timeout_ms=30000"):
                    return command_result()
                raise AssertionError(path)

        transport = LegacyTransport()
        desktop = Desktop(
            transport,
            DESKTOP_ID,
            GENERATION,
            {"major": 1, "minor": 0},
        )
        await desktop.windows.wait({"timeout_ms": 300_000})
        await desktop.accessibility.wait({"timeout_ms": 120_000})
        command = CommandHandle(
            transport,
            DESKTOP_ID,
            GENERATION,
            command_result(),
        )
        await command.wait_once(30)
        self.assertEqual(len(transport.calls), 3)

    async def test_legacy_transport_local_wait_deadline_propagates_cancellation(
        self,
    ) -> None:
        class LegacyTransport:
            base_url = "https://xenoteer.test"

            def __init__(self) -> None:
                self.started = asyncio.Event()
                self.cancelled = asyncio.Event()

            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del method, path, body, headers
                self.started.set()
                try:
                    await asyncio.Future()
                finally:
                    self.cancelled.set()
                raise AssertionError("unreachable")

        transport = LegacyTransport()
        desktop = Desktop(
            transport,
            DESKTOP_ID,
            GENERATION,
            {"major": 1, "minor": 0},
        )
        task = asyncio.create_task(desktop.windows.wait({"timeout_ms": 1}))
        await asyncio.wait_for(transport.started.wait(), timeout=1)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        await asyncio.wait_for(transport.cancelled.wait(), timeout=1)

    async def test_legacy_transport_local_deadline_expires_with_structured_error(
        self,
    ) -> None:
        class LegacyTransport:
            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del method, path, body, headers
                try:
                    await asyncio.Future()
                finally:
                    cancelled.set()
                raise AssertionError("unreachable")

        cancelled = asyncio.Event()
        with self.assertRaises(XenoteerError) as caught:
            await request_with_deadline(
                LegacyTransport(),
                "GET",
                "/v1/status",
                timeout=0.001,
            )
        self.assertEqual(caught.exception.code, "request_timeout")
        self.assertTrue(cancelled.is_set())

    async def test_deadline_capable_transport_remains_under_an_absolute_deadline(
        self,
    ) -> None:
        transport = DeadlineFailureTransport()
        with self.assertRaises(XenoteerError) as caught:
            await request_with_deadline(
                transport,
                "GET",
                "/v1/status",
                timeout=0.001,
            )
        self.assertEqual(caught.exception.code, "request_timeout")

        transport = DeadlineFailureTransport(TimeoutError("internal timeout"))
        with self.assertRaises(XenoteerError) as internal:
            await request_with_deadline(
                transport,
                "GET",
                "/v1/status",
                timeout=1,
            )
        self.assertEqual(internal.exception.code, "request_timeout")

    async def test_deadline_capable_transport_preserves_caller_cancellation(
        self,
    ) -> None:
        transport = DeadlineFailureTransport()
        task = asyncio.create_task(
            request_with_deadline(
                transport,
                "GET",
                "/v1/status",
                timeout=1,
            )
        )
        await asyncio.sleep(0)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task

    async def test_control_scopes_preserve_cancellation_while_release_finishes(self) -> None:
        class CancellationTransport:
            base_url = "https://xenoteer.test"

            def __init__(self) -> None:
                self.release_started = asyncio.Event()
                self.allow_release = asyncio.Event()
                self.release_finished = asyncio.Event()

            async def authorization_header(self) -> str:
                return f"Bearer {TOKEN}"

            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del body, headers
                if method == "POST" and path.endswith("/lease"):
                    return {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "state": "held_by_caller",
                        "lease_id": LEASE_ID,
                        "expires_at": "2030-01-01T00:01:00Z",
                    }
                if method == "DELETE" and "/lease/" in path:
                    self.release_started.set()
                    await self.allow_release.wait()
                    self.release_finished.set()
                    return {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "state": "vacant",
                        "lease_id": None,
                        "expires_at": None,
                    }
                raise AssertionError((method, path))

            async def close(self) -> None:
                pass

        for kind in ("lease", "control"):
            with self.subTest(kind=kind):
                transport = CancellationTransport()
                desktop = Desktop(
                    transport,
                    DESKTOP_ID,
                    GENERATION,
                    {"major": 1, "minor": 0},
                )
                scope: Any
                if kind == "lease":
                    scope = ControlLease(
                        desktop,
                        transport,
                        {
                            "desktop_id": DESKTOP_ID,
                            "desktop_generation": GENERATION,
                            "state": "held_by_caller",
                            "lease_id": LEASE_ID,
                            "expires_at": "2030-01-01T00:01:00Z",
                        },
                        None,
                    )
                else:
                    scope = desktop.control(ttl=60)

                async def use_scope() -> None:
                    async with scope:
                        pass

                task = asyncio.create_task(use_scope())
                await asyncio.wait_for(transport.release_started.wait(), timeout=1)
                task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                transport.allow_release.set()
                await asyncio.wait_for(transport.release_finished.wait(), timeout=1)

    async def test_application_launch_serializes_nonempty_string_arguments(self) -> None:
        def responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/commands"):
                return command_result(body["command_id"])
            raise AssertionError((method, path))

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        arguments = ["--fixture-mode", "العربية"]
        submission = client.desktop().applications.launch("editor", arguments)

        self.assertEqual(
            submission.envelope["command"],
            {
                "type": "application_launch",
                "application": "editor",
                "arguments": arguments,
            },
        )
        await submission.send()
        sent = transport.calls[-1][2]
        self.assertEqual(sent["command"]["arguments"], arguments)
        await client.close()

    async def test_keyboard_text_insertions_nest_the_complete_text_target(self) -> None:
        element_ref = {
            "desktop_id": DESKTOP_ID,
            "desktop_generation": GENERATION,
            "atspi_generation": "4",
            "application": {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "atspi_generation": "4",
                "unique_bus_name": ":1.42",
                "root_object_path": "/root",
                "app_instance_generation": "2",
                "identity_hash": "b" * 64,
            },
            "object_path": "/entry",
            "object_identity_hash": "c" * 64,
            "cache_sequence": "78",
        }

        def responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/lease"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "held_by_caller",
                    "lease_id": LEASE_ID,
                    "expires_at": "2026-07-23T00:01:00Z",
                }
            if method == "DELETE" and "/lease/" in path:
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "vacant",
                    "lease_id": None,
                    "expires_at": None,
                }
            raise AssertionError((method, path))

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        lease = await client.desktop().acquire_control()
        target = {
            "target": "element",
            "element": element_ref,
            "window_fallback": None,
        }
        submission = lease.keyboard.insert_text(
            "Latin — العربية — 中文 — e\u0301 — 😀",
            target,
            strategy="semantic",
        )
        command = submission.envelope["command"]

        self.assertEqual(command["target"], target)
        self.assertNotIn("element", command)
        self.assertNotIn("window_fallback", command)
        self.assertEqual(command["semantic_options"]["insertion_point"], {"kind": "caret"})
        self.assertIs(command["semantic_options"]["verify_length_only"], True)
        self.assertIsNone(command["clipboard_options"])

        exact_submission = lease.keyboard.insert_text(
            "exact semantic evidence",
            target,
            strategy="semantic",
            verify_length_only=False,
        )
        self.assertIs(
            exact_submission.envelope["command"]["semantic_options"]["verify_length_only"],
            False,
        )

        for invalid in (None, 0, 1, "false"):
            with self.subTest(verify_length_only=invalid):
                with self.assertRaisesRegex(
                    XenoteerError, "verify_length_only must be a bool"
                ) as caught:
                    lease.keyboard.insert_text(
                        "invalid semantic evidence mode",
                        target,
                        strategy="semantic",
                        verify_length_only=invalid,
                    )
                self.assertEqual(caught.exception.code, "invalid_request")

        artifact = ArtifactRef.from_wire(
            {
                "artifact_id": "10000000-0000-4000-8000-000000000005",
                "purpose": "clipboard_input",
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "content_type": "text/plain;charset=utf-8",
                "content_length": 24,
                "sha256": "d" * 64,
                "created_at": "2026-07-23T00:00:00Z",
                "expires_at": "2026-07-23T00:01:00Z",
            },
            desktop_id=DESKTOP_ID,
            desktop_generation=GENERATION,
            purpose="clipboard_input",
        )
        artifact_submission = lease.keyboard.insert_artifact(
            artifact,
            target,
            strategy="clipboard",
        )
        artifact_command = artifact_submission.envelope["command"]

        self.assertEqual(artifact_command["target"], target)
        self.assertNotIn("element", artifact_command)
        self.assertNotIn("window_fallback", artifact_command)
        await client.close()

    async def test_client_close_releases_owned_lease_without_masking_body_error(
        self,
    ) -> None:
        calls: list[tuple[str, str]] = []

        def successful_responder(method, path, body, headers):
            calls.append((method, path))
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/lease"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "held_by_caller",
                    "lease_id": LEASE_ID,
                    "expires_at": "2026-07-23T00:01:00Z",
                }
            if method == "DELETE" and "/lease/" in path:
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "vacant",
                    "lease_id": None,
                    "expires_at": None,
                }
            raise AssertionError((method, path))

        transport = FakeTransport(successful_responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        lease = await client.desktop().acquire_control()
        await client.close()
        self.assertFalse(lease.requires_cleanup)
        self.assertEqual(
            len([call for call in calls if call[0] == "DELETE" and "/lease/" in call[1]]),
            1,
        )

        def failing_responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/lease"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "held_by_caller",
                    "lease_id": LEASE_ID,
                    "expires_at": "2026-07-23T00:01:00Z",
                }
            if method == "DELETE" and "/lease/" in path:
                raise XenoteerError("transport", "release failed")
            raise AssertionError((method, path))

        failing_transport = FakeTransport(failing_responder)
        failing_client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN),
            transport=failing_transport,
            transport_ownership="client",
        )
        caught: RuntimeError | None = None
        try:
            async with failing_client:
                await failing_client.desktop().acquire_control()
                raise RuntimeError("primary body failure")
        except RuntimeError as error:
            caught = error
        self.assertIsNotNone(caught)
        self.assertIn(
            "Xenoteer client cleanup failed",
            "\n".join(getattr(caught, "__notes__", [])),
        )
        self.assertTrue(failing_transport.closed)

    async def test_context_releases_after_error_and_motion_is_smooth(self) -> None:
        calls: list[tuple[str, str, object]] = []

        def responder(method, path, body, headers):
            calls.append((method, path, body))
            if path == "/v1/status":
                return status()
            if method == "POST" and path.endswith("/lease"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "held_by_caller",
                    "lease_id": LEASE_ID,
                    "expires_at": "2026-07-23T00:01:00Z",
                }
            if method == "POST" and path.endswith("/commands"):
                return command_result(body["command_id"])
            if method == "DELETE" and "/lease/" in path:
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "state": "vacant",
                    "lease_id": None,
                    "expires_at": None,
                }
            raise AssertionError((method, path))

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        with self.assertRaisesRegex(RuntimeError, "body failed"):
            async with client.desktop().control(ttl=60) as control:
                await control.mouse.move(120, 300, duration=0.25)
                raise RuntimeError("body failed")
        envelopes = [
            call[2] for call in calls if call[0] == "POST" and call[1].endswith("/commands")
        ]
        self.assertEqual(envelopes[0]["command"]["curve"], "smooth")
        self.assertEqual(envelopes[0]["command"]["duration_ms"], 250)
        self.assertEqual(envelopes[0]["lease_id"], LEASE_ID)
        self.assertEqual(
            len([call for call in calls if call[0] == "DELETE" and "/lease/" in call[1]]),
            1,
        )
        await client.close()

    async def test_window_accessibility_clipboard_capture_and_app_paths(self) -> None:
        window_ref = {
            "desktop_id": DESKTOP_ID,
            "desktop_generation": GENERATION,
            "xid": 42,
            "observed_generation": "9007199254740993",
            "identity_hash": "a" * 64,
        }
        element_ref = {
            "desktop_id": DESKTOP_ID,
            "desktop_generation": GENERATION,
            "atspi_generation": "4",
            "application": {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "atspi_generation": "4",
                "unique_bus_name": ":1.42",
                "root_object_path": "/root",
                "app_instance_generation": "2",
                "identity_hash": "b" * 64,
            },
            "object_path": "/button",
            "object_identity_hash": "c" * 64,
            "cache_sequence": "78",
        }

        def responder(method, path, body, headers):
            if path == "/v1/status":
                return status()
            if path.endswith("/windows/resolve"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "snapshot_revision": "9",
                    "window": {
                        "snapshot": {"ref": window_ref, "model_revision": "9"},
                        "reference_token": "a" * 32,
                    },
                }
            if path.endswith("/accessibility/elements/resolve"):
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "atspi_generation": "4",
                    "snapshot_revision": "9",
                    "element": {"snapshot": {"ref": element_ref, "revision": "9"}},
                }
            if "/clipboard/read?" in path:
                return {"selection": "clipboard", "revision": "1"}
            if "/screenshots?" in path:
                return {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "artifact": {"content_length": 12},
                }
            if path.endswith("/commands"):
                return command_result(body["command_id"])
            raise AssertionError((method, path))

        transport = FakeTransport(responder)
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        desktop = client.desktop()
        window = await desktop.windows.one({"type": "all"})
        self.assertEqual(window.ref["observed_generation"], "9007199254740993")
        await window.activate()
        element = await desktop.accessibility.one({"type": "all"})
        await element.invoke()
        self.assertEqual((await desktop.clipboard.read())["revision"], "1")
        await desktop.capture.screenshot()
        await desktop.applications.launch("editor")
        await client.close()


class MockSocket:
    def __init__(
        self,
        receives: list[object],
        *,
        auto_ack: bool = False,
        after_ack: list[object] | None = None,
    ) -> None:
        self.receives: asyncio.Queue[object] = asyncio.Queue()
        for item in receives:
            self.receives.put_nowait(item)
        self.sent: list[dict[str, Any]] = []
        self.closed = False
        self.auto_ack = auto_ack
        self.after_ack = [] if after_ack is None else after_ack

    async def send(self, message: str) -> None:
        decoded = json.loads(message)
        self.sent.append(decoded)
        if self.auto_ack and decoded.get("type") == "events.subscribe":
            self.receives.put_nowait(
                json.dumps(
                    {
                        "type": "events.subscribed",
                        "request_id": decoded["request_id"],
                        "topics": decoded["topics"],
                    }
                )
            )
            for item in self.after_ack:
                self.receives.put_nowait(item)
            self.after_ack.clear()

    async def recv(self) -> str:
        item = await self.receives.get()
        if isinstance(item, Exception):
            raise item
        return item  # type: ignore[return-value]

    async def close(self, **kwargs: Any) -> None:
        self.closed = True


class EventSessionTests(unittest.IsolatedAsyncioTestCase):
    async def test_header_handshake_unknown_event_and_no_token_in_url(self) -> None:
        socket = MockSocket([welcome_message()])
        factory_calls: list[tuple[str, dict[str, str]]] = []

        async def factory(url, *, additional_headers, **kwargs):
            factory_calls.append((url, additional_headers))
            return socket

        transport = FakeTransport(lambda method, path, body, headers: status())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        session = await client.open_events(websocket_factory=factory)
        subscribe = asyncio.create_task(session.subscribe(DESKTOP_ID, GENERATION, ["future.topic"]))
        while len(socket.sent) < 2:
            await asyncio.sleep(0)
        request_id = socket.sent[1]["request_id"]
        socket.receives.put_nowait(
            json.dumps(
                {
                    "type": "events.subscribed",
                    "request_id": request_id,
                    "topics": ["future.topic"],
                }
            )
        )
        await subscribe
        socket.receives.put_nowait(
            json.dumps(
                {
                    "type": "event",
                    "request_id": request_id,
                    "event": {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "sequence": "1",
                        "topic": "future.topic",
                        "payload": {"future": True},
                    },
                }
            )
        )
        event = await asyncio.wait_for(anext(session), timeout=1)
        self.assertIsInstance(event, UnknownEvent)
        self.assertNotIn(TOKEN, factory_calls[0][0])
        self.assertEqual(factory_calls[0][1]["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(socket.sent[0]["type"], "client.hello")
        self.assertNotIn(TOKEN, repr(session))
        await client.close()

    async def test_zero_cursor_is_valid_for_hello_subscription_and_comparison(
        self,
    ) -> None:
        socket = MockSocket([welcome_message()])

        async def factory(url, **kwargs):
            return socket

        transport = FakeTransport(lambda method, path, body, headers: status())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN), transport=transport
        )
        session = await client.open_events(
            resume={
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "event_sequence": "0",
            },
            websocket_factory=factory,
        )
        self.assertEqual(socket.sent[0]["resume"]["event_sequence"], "0")
        subscribe = asyncio.create_task(
            session.subscribe(
                DESKTOP_ID,
                GENERATION,
                ["window.created"],
                since_sequence=0,
            )
        )
        while len(socket.sent) < 2:
            await asyncio.sleep(0)
        request_id = socket.sent[1]["request_id"]
        self.assertEqual(socket.sent[1]["since_sequence"], "0")
        socket.receives.put_nowait(
            json.dumps(
                {
                    "type": "events.subscribed",
                    "request_id": request_id,
                    "topics": ["window.created"],
                }
            )
        )
        socket.receives.put_nowait(
            json.dumps(
                {
                    "type": "event",
                    "request_id": request_id,
                    "event": {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "sequence": "1",
                        "topic": "window.created",
                        "payload": {},
                    },
                }
            )
        )
        await subscribe
        self.assertEqual((await asyncio.wait_for(anext(session), 1)).sequence, 1)
        self.assertEqual(session.resume_cursor, "1")
        await client.close()

    async def test_event_requires_active_matching_subscription_contract(self) -> None:
        async def exercise(
            *,
            request_id: str,
            desktop_id: str = DESKTOP_ID,
            generation: str = GENERATION,
            topic: str = "window.created",
            subscribe_first: bool = True,
        ) -> str:
            socket = MockSocket([welcome_message()])

            async def factory(url, **kwargs):
                return socket

            transport = FakeTransport(lambda method, path, body, headers: status())
            client = await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", TOKEN),
                transport=transport,
            )
            session = await client.open_events(websocket_factory=factory)
            active_request = request_id
            if subscribe_first:
                subscribe = asyncio.create_task(
                    session.subscribe(DESKTOP_ID, GENERATION, ["window.created"])
                )
                while len(socket.sent) < 2:
                    await asyncio.sleep(0)
                active_request = socket.sent[1]["request_id"]
                socket.receives.put_nowait(
                    json.dumps(
                        {
                            "type": "events.subscribed",
                            "request_id": active_request,
                            "topics": ["window.created"],
                        }
                    )
                )
                await subscribe
            socket.receives.put_nowait(
                json.dumps(
                    {
                        "type": "event",
                        "request_id": (active_request if request_id == "<active>" else request_id),
                        "event": {
                            "desktop_id": desktop_id,
                            "desktop_generation": generation,
                            "sequence": "1",
                            "topic": topic,
                            "payload": {},
                        },
                    }
                )
            )
            try:
                with self.assertRaises(XenoteerError) as rejected:
                    await asyncio.wait_for(anext(session), 1)
                return rejected.exception.code
            finally:
                await client.close()

        for case in (
            {"request_id": COMMAND_ID, "subscribe_first": False},
            {"request_id": COMMAND_ID},
            {"request_id": "<active>", "desktop_id": LEASE_ID},
            {"request_id": "<active>", "generation": LEASE_ID},
            {"request_id": "<active>", "topic": "process.exited"},
        ):
            with self.subTest(case=case):
                self.assertIn(
                    await exercise(**case),
                    {"invalid_response", "generation_changed"},
                )

    async def test_overflow_cursor_and_reserved_resync_reason_are_honest(self) -> None:
        async def open_session(frames):
            socket = MockSocket([welcome_message()])

            async def factory(url, **kwargs):
                return socket

            transport = FakeTransport(lambda method, path, body, headers: status())
            client = await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", TOKEN),
                transport=transport,
            )
            session = await client.open_events(capacity=1, websocket_factory=factory)
            subscribe = asyncio.create_task(
                session.subscribe(DESKTOP_ID, GENERATION, ["window.created"])
            )
            while len(socket.sent) < 2:
                await asyncio.sleep(0)
            request_id = socket.sent[1]["request_id"]
            socket.receives.put_nowait(
                json.dumps(
                    {
                        "type": "events.subscribed",
                        "request_id": request_id,
                        "topics": ["window.created"],
                    }
                )
            )
            await subscribe
            for frame in frames(request_id):
                socket.receives.put_nowait(json.dumps(frame))
            while not session._reader.done():
                await asyncio.sleep(0)
            return client, session

        def event(request_id: str, sequence: str) -> dict[str, Any]:
            return {
                "type": "event",
                "request_id": request_id,
                "event": {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "sequence": sequence,
                    "topic": "window.created",
                    "payload": {},
                },
            }

        overflow_client, overflow = await open_session(
            lambda request_id: [event(request_id, "1"), event(request_id, "2")]
        )
        self.assertEqual((await anext(overflow)).sequence, 1)
        self.assertEqual(overflow.resume_cursor, "1")
        with self.assertRaises(XenoteerError) as full:
            await anext(overflow)
        self.assertEqual(full.exception.code, "backpressure")
        await overflow_client.close()

        resync_client, resync = await open_session(
            lambda request_id: [
                event(request_id, "1"),
                {
                    "type": "events.resync_required",
                    "request_id": request_id,
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "reason": "history_lost",
                    "dropped_through": "1",
                    "latest_sequence": "2",
                },
            ]
        )
        self.assertEqual((await anext(resync)).sequence, 1)
        barrier = await anext(resync)
        self.assertIsInstance(barrier, ResyncRequired)
        self.assertEqual(barrier.reason, "history_lost")
        self.assertEqual(resync.resume_cursor, "1")
        with self.assertRaises(XenoteerError) as lost:
            await anext(resync)
        self.assertEqual(lost.exception.code, "resync_required")
        await resync_client.close()

    async def test_sequence_regression_is_structured_and_duplicate_is_ignored(
        self,
    ) -> None:
        socket = MockSocket([welcome_message()])

        async def factory(url, **kwargs):
            return socket

        transport = FakeTransport(lambda method, path, body, headers: status())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", TOKEN),
            transport=transport,
        )
        session = await client.open_events(capacity=1, websocket_factory=factory)
        subscribe = asyncio.create_task(
            session.subscribe(DESKTOP_ID, GENERATION, ["window.created"])
        )
        while len(socket.sent) < 2:
            await asyncio.sleep(0)
        request_id = socket.sent[1]["request_id"]
        socket.receives.put_nowait(
            json.dumps(
                {
                    "type": "events.subscribed",
                    "request_id": request_id,
                    "topics": ["window.created"],
                }
            )
        )
        await subscribe

        def event(sequence: str) -> dict[str, Any]:
            return {
                "type": "event",
                "request_id": request_id,
                "event": {
                    "desktop_id": DESKTOP_ID,
                    "desktop_generation": GENERATION,
                    "sequence": sequence,
                    "topic": "window.created",
                    "payload": {},
                },
            }

        offending = event("4")
        socket.receives.put_nowait(json.dumps(event("5")))
        socket.receives.put_nowait(json.dumps(event("5")))
        socket.receives.put_nowait(json.dumps(offending))

        admitted = await asyncio.wait_for(anext(session), timeout=1)
        self.assertEqual(admitted.sequence, 5)
        marker = await asyncio.wait_for(anext(session), timeout=1)
        self.assertIsInstance(marker, ResyncRequired)
        self.assertEqual(marker.reason, "sequence_regression")
        self.assertEqual(marker.dropped_through, 4)
        self.assertEqual(marker.latest_sequence, 5)
        self.assertEqual(marker.raw, offending)
        self.assertEqual(session.resume_cursor, "5")
        with self.assertRaises(XenoteerError) as terminal:
            await asyncio.wait_for(anext(session), timeout=1)
        self.assertEqual(terminal.exception.code, "resync_required")
        await client.close()

    async def test_reconnect_resumes_last_sequence_without_command_replay(self) -> None:
        def event(request_id: str, sequence: str, topic: str) -> str:
            return json.dumps(
                {
                    "type": "event",
                    "request_id": request_id,
                    "event": {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "sequence": sequence,
                        "topic": topic,
                        "payload": {},
                    },
                }
            )

        first = MockSocket([welcome_message()])
        second = MockSocket([welcome_message()], auto_ack=True)
        sockets = [first, second]

        async def factory(url, **kwargs):
            return sockets.pop(0)

        transport = FakeTransport(lambda method, path, body, headers: status())
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                TOKEN,
                heartbeat_interval=300,
                reconnect_policy=ReconnectPolicy(max_attempts=1),
            ),
            transport=transport,
        )
        session = await client.open_events(websocket_factory=factory)
        subscribe = asyncio.create_task(
            session.subscribe(DESKTOP_ID, GENERATION, ["window.created"])
        )
        while len(first.sent) < 2:
            await asyncio.sleep(0)
        first.receives.put_nowait(
            json.dumps(
                {
                    "type": "events.subscribed",
                    "request_id": first.sent[1]["request_id"],
                    "topics": ["window.created"],
                }
            )
        )
        first.receives.put_nowait(event(first.sent[1]["request_id"], "41", "window.created"))
        first.receives.put_nowait(ConnectionError("drop"))
        await subscribe
        self.assertEqual((await asyncio.wait_for(anext(session), 1)).sequence, 41)
        while len(second.sent) < 2:
            await asyncio.sleep(0)
        second.receives.put_nowait(event(second.sent[1]["request_id"], "42", "window.created"))
        self.assertEqual((await asyncio.wait_for(anext(session), 2)).sequence, 42)
        self.assertEqual(second.sent[0]["resume"]["event_sequence"], "41")
        self.assertEqual(second.sent[1]["since_sequence"], "41")
        self.assertFalse(any(call[1].endswith("/commands") for call in transport.calls))
        await client.close()


class PackageBoundaryTests(unittest.TestCase):
    def test_lock_covers_every_declared_requirement_and_build_tool(self) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        configuration = tomllib.loads((root / "pyproject.toml").read_text())
        declared = [
            *configuration["build-system"]["requires"],
            *configuration["project"]["dependencies"],
            *configuration["project"]["optional-dependencies"]["dev"],
        ]
        pins = {
            canonicalize_name(name): Version(version.split()[0])
            for line in (root / "requirements-test.lock").read_text().splitlines()
            if line and not line.startswith("#")
            for name, separator, version in [line.partition("==")]
            if separator
        }
        self.assertIn(canonicalize_name("setuptools"), pins)
        self.assertIn(canonicalize_name("wheel"), pins)
        for value in declared:
            requirement = Requirement(value)
            name = canonicalize_name(requirement.name)
            self.assertIn(name, pins, value)
            self.assertTrue(
                requirement.specifier.contains(pins[name], prereleases=True),
                f"{name}=={pins[name]} does not satisfy {requirement.specifier}",
            )

    def test_reviewed_allowlist_and_apache_boundary(self) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        allowlist = {
            line
            for line in (root / "PACKAGE_ALLOWLIST.txt").read_text().splitlines()
            if line and not line.startswith("#")
        }
        expected = {
            path.relative_to(root).as_posix()
            for path in root.rglob("*")
            if path.is_file()
            and not any(
                part
                in {
                    "__pycache__",
                    ".mypy_cache",
                    ".pytest_cache",
                    ".ruff_cache",
                    ".venv",
                    "build",
                    "dist",
                }
                or part.endswith(".egg-info")
                for part in path.parts
            )
            and path.name != ".gitignore"
            and not path.name.endswith((".pyc", ".pyo"))
        }
        self.assertEqual(allowlist, expected)
        pyproject = (root / "pyproject.toml").read_text()
        self.assertIn('license = "Apache-2.0"', pyproject)
        self.assertIn('license-files = ["LICENSE", "NOTICE"]', pyproject)
        for source in (root / "src").rglob("*.py"):
            text = source.read_text()
            self.assertNotIn("xenoteer_server", text)
            self.assertIn(
                "SP" + "DX-License-Identifier: Apache-2.0",
                text,
            )

    def test_distribution_verifier_rejects_links_and_non_apache_sources(self) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        spec = importlib.util.spec_from_file_location(
            "xenoteer_verify_dist", root / "scripts" / "verify_dist.py"
        )
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            archive_path = pathlib.Path(directory) / "bad.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                link = tarfile.TarInfo("xenoteer-0.1.0/src/xenoteer/link.py")
                link.type = tarfile.SYMTYPE
                link.linkname = "/tmp/server.py"
                archive.addfile(link)
            with self.assertRaisesRegex(RuntimeError, "non-regular"):
                module.verify_sdist(archive_path)

        identifier = b"SP" + b"DX-License-Identifier"
        invalid_sources = (
            b"# " + identifier + b": GPL-3.0-only\n",
            (
                b"# " + identifier + b": Apache-2.0\n"
                b"# " + identifier + b": GPL-3.0-only\n"
            ),
            b"# " + identifier + b": Apache-2.0 AND MIT\n",
            b"# " + identifier + b": Apache-2.0 OR MIT\n",
            b"# " + identifier + b": LicenseRef-Proprietary\n",
            (
                b"# " + identifier + b": Apache-2.0\n"
                b"# " + identifier + b": Apache-2.0\n"
            ),
            b"# " + (b"sp" + b"dx-license-identifier") + b": Apache-2.0\n",
            b"# " + identifier + b" : Apache-2.0\n",
            (
                b"# " + identifier + b": Apache-2.0\n"
                b"payload = '" + identifier + b": MIT'\n"
            ),
        )
        for source in invalid_sources:
            with self.subTest(source=source):
                with self.assertRaisesRegex(RuntimeError, "SPDX|Apache"):
                    module._verify_python_sources({"xenoteer/hidden.py": source})

    def test_wheel_and_sdist_reject_every_spdx_expression_smuggling_form(
        self,
    ) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        spec = importlib.util.spec_from_file_location(
            "xenoteer_verify_dist_archives", root / "scripts" / "verify_dist.py"
        )
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        identifier = b"SP" + b"DX-License-Identifier"
        valid_source = (
            b"# "
            + identifier
            + b": Apache-2.0\n"
            + b'"""Reviewed package fixture."""\n'
        )
        invalid_sources = (
            valid_source + b"# " + identifier + b": BUSL-1.1\n",
            valid_source + b"# " + identifier + b": GPL-3.0-only\n",
            valid_source + b"# " + identifier + b": MIT\n",
            valid_source + b"# " + identifier + b": AGPL-3.0-only\n",
            valid_source + b"# " + identifier + b": LicenseRef-Private\n",
            valid_source + b"# " + identifier + b": Apache-2.0 AND MIT\n",
            valid_source + b"# " + identifier + b": Apache-2.0 OR MIT\n",
            valid_source + b"# " + identifier + b": Apache-2.0\n",
            valid_source
            + b"VALUE = '"
            + (b"sp" + b"dx-license-identifier")
            + b": MIT'\n",
        )

        def wheel_member(normalized: str) -> str:
            return normalized.replace(
                "DIST_INFO", "xenoteer-0.1.0.dist-info", 1
            )

        def sdist_member(normalized: str) -> str:
            if normalized.startswith("EGG_INFO/"):
                normalized = normalized.replace(
                    "EGG_INFO/", "src/xenoteer.egg-info/", 1
                )
            return f"xenoteer-0.1.0/{normalized}"

        def contents(name: str, source: bytes) -> bytes:
            if name.endswith(".py"):
                return source if name.endswith("xenoteer/__init__.py") else valid_source
            if name.endswith(("METADATA", "PKG-INFO")):
                return (
                    b"License-Expression: Apache-2.0\n"
                    b"Requires-Dist: httpx\n"
                    b"Requires-Dist: websockets\n"
                )
            return b""

        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            for index, source in enumerate(invalid_sources):
                wheel_path = temporary / f"invalid-{index}.whl"
                with ZipFile(wheel_path, "w") as archive:
                    for normalized in sorted(module._allowlist("WHEEL_ALLOWLIST.txt")):
                        archive.writestr(
                            wheel_member(normalized),
                            contents(normalized, source),
                        )
                with self.subTest(kind="wheel", source=source):
                    with self.assertRaisesRegex(RuntimeError, "SPDX|Apache|BSL"):
                        module.verify_wheel(wheel_path)

                sdist_path = temporary / f"invalid-{index}.tar.gz"
                with tarfile.open(sdist_path, "w:gz") as archive:
                    for normalized in sorted(module._allowlist("SDIST_ALLOWLIST.txt")):
                        payload = contents(normalized, source)
                        member = tarfile.TarInfo(sdist_member(normalized))
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                with self.subTest(kind="sdist", source=source):
                    with self.assertRaisesRegex(RuntimeError, "SPDX|Apache|BSL"):
                        module.verify_sdist(sdist_path)

    def test_wheel_and_sdist_reject_duplicate_and_aliased_member_names(
        self,
    ) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        spec = importlib.util.spec_from_file_location(
            "xenoteer_verify_dist_duplicates",
            root / "scripts" / "verify_dist.py",
        )
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        identifier = b"SP" + b"DX-License-Identifier"
        valid_source = b"# " + identifier + b": Apache-2.0\n"
        malicious_source = b"# " + identifier + b": GPL-3.0-only\n"
        metadata = (
            b"License-Expression: Apache-2.0\n"
            b"Requires-Dist: httpx\n"
            b"Requires-Dist: websockets\n"
        )

        def contents(name: str) -> bytes:
            if name.endswith(".py"):
                return valid_source
            if name.endswith(("METADATA", "PKG-INFO")):
                return metadata
            return b"reviewed\n"

        def wheel_member(normalized: str) -> str:
            return normalized.replace(
                "DIST_INFO",
                "xenoteer-0.1.0.dist-info",
                1,
            )

        def sdist_member(normalized: str) -> str:
            if normalized.startswith("EGG_INFO/"):
                normalized = normalized.replace(
                    "EGG_INFO/",
                    "src/xenoteer.egg-info/",
                    1,
                )
            return f"xenoteer-0.1.0/{normalized}"

        duplicate_cases = (
            (
                "malicious-first",
                "xenoteer/__init__.py",
                (malicious_source, valid_source),
            ),
            (
                "malicious-last",
                "xenoteer/__init__.py",
                (valid_source, malicious_source),
            ),
            (
                "same-valid-source",
                "xenoteer/__init__.py",
                (valid_source, valid_source),
            ),
            (
                "license",
                "DIST_INFO/licenses/LICENSE",
                (b"reviewed\n", b"reviewed\n"),
            ),
            (
                "metadata",
                "DIST_INFO/METADATA",
                (metadata, metadata),
            ),
        )
        sdist_duplicate_cases = (
            (
                "malicious-first",
                "src/xenoteer/__init__.py",
                (malicious_source, valid_source),
            ),
            (
                "malicious-last",
                "src/xenoteer/__init__.py",
                (valid_source, malicious_source),
            ),
            (
                "same-valid-source",
                "src/xenoteer/__init__.py",
                (valid_source, valid_source),
            ),
            ("license", "LICENSE", (b"reviewed\n", b"reviewed\n")),
            ("metadata", "PKG-INFO", (metadata, metadata)),
        )
        aliases = (
            "xenoteer/./__init__.py",
            "./xenoteer/__init__.py",
            "xenoteer//__init__.py",
            "xenoteer/../xenoteer/__init__.py",
            "xenoteer\\__init__.py",
        )

        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            wheel_allowlist = sorted(module._allowlist("WHEEL_ALLOWLIST.txt"))
            for index, (label, target, payloads) in enumerate(duplicate_cases):
                wheel_path = temporary / f"duplicate-{index}.whl"
                with ZipFile(wheel_path, "w") as archive:
                    for normalized in wheel_allowlist:
                        actual = wheel_member(normalized)
                        if normalized == target:
                            for payload in payloads:
                                archive.writestr(actual, payload)
                        else:
                            archive.writestr(actual, contents(normalized))
                with self.subTest(kind="wheel", duplicate=label):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "duplicate|unsafe|boundary|SPDX|Apache",
                    ):
                        module.verify_wheel(wheel_path)

            for index, alias in enumerate(aliases):
                wheel_path = temporary / f"alias-{index}.whl"
                with ZipFile(wheel_path, "w") as archive:
                    for normalized in wheel_allowlist:
                        archive.writestr(
                            wheel_member(normalized),
                            contents(normalized),
                        )
                    archive.writestr(alias, valid_source)
                with self.subTest(kind="wheel", alias=alias):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "duplicate|unsafe|boundary",
                    ):
                        module.verify_wheel(wheel_path)

            sdist_allowlist = sorted(module._allowlist("SDIST_ALLOWLIST.txt"))
            for index, (label, target, payloads) in enumerate(
                sdist_duplicate_cases
            ):
                sdist_path = temporary / f"duplicate-{index}.tar.gz"
                with tarfile.open(sdist_path, "w:gz") as archive:
                    for normalized in sdist_allowlist:
                        actual = sdist_member(normalized)
                        selected = (
                            payloads
                            if normalized == target
                            else (contents(normalized),)
                        )
                        for payload in selected:
                            member = tarfile.TarInfo(actual)
                            member.size = len(payload)
                            archive.addfile(member, io.BytesIO(payload))
                with self.subTest(kind="sdist", duplicate=label):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "duplicate|unsafe|boundary|SPDX|Apache",
                    ):
                        module.verify_sdist(sdist_path)

            for index, alias in enumerate(aliases):
                sdist_path = temporary / f"alias-{index}.tar.gz"
                with tarfile.open(sdist_path, "w:gz") as archive:
                    for normalized in sdist_allowlist:
                        payload = contents(normalized)
                        member = tarfile.TarInfo(sdist_member(normalized))
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                    payload = valid_source
                    member = tarfile.TarInfo(f"xenoteer-0.1.0/{alias}")
                    member.size = len(payload)
                    archive.addfile(member, io.BytesIO(payload))
                with self.subTest(kind="sdist", alias=alias):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "duplicate|unsafe|boundary",
                    ):
                        module.verify_sdist(sdist_path)

    def test_wheel_and_sdist_reject_alternative_normalized_identities(
        self,
    ) -> None:
        root = pathlib.Path(__file__).resolve().parents[1]
        spec = importlib.util.spec_from_file_location(
            "xenoteer_verify_dist_identities",
            root / "scripts" / "verify_dist.py",
        )
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        identifier = b"SP" + b"DX-License-Identifier"
        valid_source = b"# " + identifier + b": Apache-2.0\n"
        metadata = (
            b"License-Expression: Apache-2.0\n"
            b"Requires-Dist: httpx\n"
            b"Requires-Dist: websockets\n"
        )

        def contents(name: str) -> bytes:
            if name.endswith(".py"):
                return valid_source
            if name.endswith(("METADATA", "PKG-INFO")):
                return metadata
            return b"reviewed\n"

        def wheel_member(normalized: str, version: str) -> str:
            return normalized.replace(
                "DIST_INFO",
                f"xenoteer-{version}.dist-info",
                1,
            )

        def sdist_member(normalized: str, version: str) -> str:
            if normalized.startswith("EGG_INFO/"):
                normalized = normalized.replace(
                    "EGG_INFO/",
                    "src/xenoteer.egg-info/",
                    1,
                )
            return f"xenoteer-{version}/{normalized}"

        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            wheel_allowlist = sorted(module._allowlist("WHEEL_ALLOWLIST.txt"))
            for label, alternative_members in (
                (
                    "full",
                    [
                        normalized
                        for normalized in wheel_allowlist
                        if normalized.startswith("DIST_INFO/")
                    ],
                ),
                ("partial", ["DIST_INFO/METADATA"]),
            ):
                wheel_path = temporary / f"alternative-{label}.whl"
                with ZipFile(wheel_path, "w") as archive:
                    for normalized in wheel_allowlist:
                        archive.writestr(
                            wheel_member(normalized, "0.1.0"),
                            contents(normalized),
                        )
                    for normalized in alternative_members:
                        archive.writestr(
                            wheel_member(normalized, "9.9.9"),
                            contents(normalized),
                        )
                with self.subTest(kind="wheel", tree=label):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "identity|duplicate|dist-info|boundary",
                    ):
                        module.verify_wheel(wheel_path)

            sdist_allowlist = sorted(module._allowlist("SDIST_ALLOWLIST.txt"))
            for label, alternative_members in (
                ("full", sdist_allowlist),
                ("partial", ["README.md"]),
            ):
                sdist_path = temporary / f"alternative-{label}.tar.gz"
                with tarfile.open(sdist_path, "w:gz") as archive:
                    for normalized in sdist_allowlist:
                        payload = contents(normalized)
                        member = tarfile.TarInfo(
                            sdist_member(normalized, "0.1.0")
                        )
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                    for normalized in alternative_members:
                        payload = contents(normalized)
                        member = tarfile.TarInfo(
                            sdist_member(normalized, "9.9.9")
                        )
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                with self.subTest(kind="sdist", tree=label):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "identity|duplicate|root|boundary",
                    ):
                        module.verify_sdist(sdist_path)
