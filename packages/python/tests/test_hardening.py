# SPDX-License-Identifier: Apache-2.0
"""Runtime transport, continuity, and conformance hardening tests."""

from __future__ import annotations

import asyncio
import copy
import hashlib
import importlib.util
import io
import json
import pathlib
import unittest
from contextlib import redirect_stderr
from collections import Counter
from collections.abc import AsyncIterator, Mapping
from typing import Any
from unittest import mock

import httpx
from websockets.asyncio.server import serve

from xenoteer import (
    ArtifactRef,
    ClientOptions,
    Desktop,
    Element,
    EventSession,
    HttpTransport,
    ReplayComplete,
    ResyncRequired,
    Window,
    XenoteerError,
)
from xenoteer.command import CommandHandle
from xenoteer.conformance import run_cases, run_v1_conformance
from xenoteer.lease import ControlLease
from xenoteer.state import GenerationRegistry


DESKTOP_ID = "10000000-0000-4000-8000-000000000001"
GENERATION = "10000000-0000-4000-8000-000000000002"
ARTIFACT_ID = "10000000-0000-4000-8000-000000000003"
LEASE_ID = "10000000-0000-4000-8000-000000000004"
COMMAND_ID = "10000000-0000-4000-8000-000000000005"
TOKEN = "t" * 48


def artifact_wire(body: bytes, purpose: str = "screenshot") -> dict[str, Any]:
    return {
        "artifact_id": ARTIFACT_ID,
        "purpose": purpose,
        "desktop_id": DESKTOP_ID,
        "desktop_generation": GENERATION,
        "content_type": "application/octet-stream",
        "content_length": len(body),
        "sha256": hashlib.sha256(body).hexdigest(),
        "created_at": "2030-01-01T00:00:00Z",
        "expires_at": "2030-01-01T01:00:00Z",
    }


def welcome_wire() -> dict[str, Any]:
    return {
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
            "heartbeat_ms": 1_000,
            "normal_outbound_capacity": 32,
            "reserved_outbound_capacity": 8,
            "max_command_watches": 16,
        },
        "resume": {"status": "not_requested"},
    }


class RuntimeHttpTests(unittest.IsolatedAsyncioTestCase):
    async def test_streamed_upload_bounds_digest_and_source_failures(self) -> None:
        body = b"bounded-stream-body"
        digest = hashlib.sha256(body).hexdigest()

        async def handler(request: httpx.Request) -> httpx.Response:
            received = await request.aread()
            wire = artifact_wire(received, "clipboard_input")
            return httpx.Response(
                201,
                headers={"content-type": "application/json"},
                json=wire,
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN), http_client=http
            )

            async def valid() -> AsyncIterator[bytes]:
                yield body[:3]
                yield body[3:9]
                yield body[9:]

            result = await transport.upload_artifact_stream(
                "/v1/artifacts?purpose=clipboard_input",
                "application/octet-stream",
                valid(),
                content_length=len(body),
                sha256=digest,
            )
            self.assertEqual(result["sha256"], digest)

            async def short() -> AsyncIterator[bytes]:
                yield body[:-1]

            with self.assertRaisesRegex(XenoteerError, "ended after"):
                await transport.upload_artifact_stream(
                    "/v1/artifacts?purpose=clipboard_input",
                    "application/octet-stream",
                    short(),
                    content_length=len(body),
                    sha256=digest,
                )

            async def long() -> AsyncIterator[bytes]:
                yield body
                yield b"!"

            with self.assertRaisesRegex(XenoteerError, "exceeds"):
                await transport.upload_artifact_stream(
                    "/v1/artifacts?purpose=clipboard_input",
                    "application/octet-stream",
                    long(),
                    content_length=len(body),
                    sha256=digest,
                )

            async def source_failure() -> AsyncIterator[bytes]:
                yield body[:2]
                raise RuntimeError("XENOTEER_STREAM_SECRET")

            with self.assertRaises(XenoteerError) as failed:
                await transport.upload_artifact_stream(
                    "/v1/artifacts?purpose=clipboard_input",
                    "application/octet-stream",
                    source_failure(),
                    content_length=len(body),
                    sha256=digest,
                )
            self.assertEqual(failed.exception.code, "artifact_input")
            self.assertNotIn("XENOTEER_STREAM_SECRET", repr(failed.exception))

            async def wrong_digest() -> AsyncIterator[bytes]:
                yield body

            with self.assertRaisesRegex(XenoteerError, "digest"):
                await transport.upload_artifact_stream(
                    "/v1/artifacts?purpose=clipboard_input",
                    "application/octet-stream",
                    wrong_digest(),
                    content_length=len(body),
                    sha256="0" * 64,
                )

    async def test_real_httpx_artifact_round_trip_and_headers(self) -> None:
        body = b"verified artifact"
        seen: list[httpx.Request] = []

        def handler(request: httpx.Request) -> httpx.Response:
            seen.append(request)
            if request.method == "POST":
                wire = artifact_wire(body, "clipboard_input")
                return httpx.Response(
                    201,
                    headers={"content-type": "application/json"},
                    json=wire,
                )
            if request.method == "GET":
                return httpx.Response(
                    200,
                    headers={
                        "content-type": "application/octet-stream",
                        "content-length": str(len(body)),
                        "x-content-sha256": hashlib.sha256(body).hexdigest(),
                    },
                    content=body,
                )
            return httpx.Response(204, headers={"content-length": "0"})

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN), http_client=http
            )
            desktop = Desktop(transport, DESKTOP_ID, GENERATION, {"major": 1, "minor": 0})
            uploaded = await desktop.artifacts.upload_clipboard_input(
                "application/octet-stream", body
            )
            screenshot = ArtifactRef.from_wire(artifact_wire(body))
            self.assertEqual(await desktop.artifacts.download_bytes(screenshot), body)
            await desktop.artifacts.delete(screenshot)
        self.assertEqual(seen[0].headers["content-length"], str(len(body)))
        self.assertEqual(seen[0].headers["x-content-sha256"], hashlib.sha256(body).hexdigest())
        self.assertEqual(uploaded.purpose, "clipboard_input")

    async def test_real_httpx_malformed_oversize_auth_and_timeout(self) -> None:
        async def request(handler, *, maximum=64):
            async with httpx.AsyncClient(
                base_url="https://xenoteer.test",
                transport=httpx.MockTransport(handler),
            ) as http:
                transport = HttpTransport(
                    ClientOptions(
                        "https://xenoteer.test",
                        TOKEN,
                        max_response_bytes=maximum,
                    ),
                    http_client=http,
                )
                return await transport.request("GET", "/v1/status")

        with self.assertRaises(XenoteerError) as oversized:
            await request(
                lambda request: httpx.Response(
                    200,
                    headers={"content-type": "application/json"},
                    content=b"x" * 65,
                )
            )
        self.assertEqual(oversized.exception.code, "response_too_large")

        with self.assertRaises(XenoteerError) as malformed:
            await request(
                lambda request: httpx.Response(
                    200,
                    headers={"content-type": "application/json"},
                    content=b"{",
                )
            )
        self.assertEqual(malformed.exception.code, "invalid_response")

        with self.assertRaises(XenoteerError) as auth:
            await request(
                lambda request: httpx.Response(
                    401,
                    headers={"content-type": "application/problem+json"},
                    json={"code": "invalid_token", "detail": TOKEN},
                ),
                maximum=1024,
            )
        self.assertEqual(auth.exception.code, "authentication")
        self.assertNotIn(TOKEN, repr(auth.exception))

        def timeout(request: httpx.Request) -> httpx.Response:
            raise httpx.ReadTimeout("secret timeout body", request=request)

        with self.assertRaises(XenoteerError) as timed_out:
            await request(timeout)
        self.assertEqual(timed_out.exception.code, "request_timeout")

    async def test_real_httpx_per_request_timeout_is_exact_and_bounded(self) -> None:
        observed: list[dict[str, float]] = []

        def handler(request: httpx.Request) -> httpx.Response:
            observed.append(dict(request.extensions["timeout"]))
            return httpx.Response(
                200,
                headers={"content-type": "application/json"},
                json={"ok": True},
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN),
                http_client=http,
            )
            for timeout in (305.0, 125.0):
                self.assertEqual(
                    await transport.request_with_timeout("GET", "/v1/status", timeout=timeout),
                    {"ok": True},
                )
            for invalid in (
                0,
                -1,
                305.000_001,
                10**10_000,
                float("inf"),
                float("nan"),
                True,
            ):
                with self.subTest(timeout=invalid), self.assertRaises(XenoteerError):
                    await transport.request_with_timeout("GET", "/v1/status", timeout=invalid)

        self.assertEqual(
            observed,
            [
                {"connect": 305.0, "read": 305.0, "write": 305.0, "pool": 305.0},
                {"connect": 125.0, "read": 125.0, "write": 125.0, "pool": 125.0},
            ],
        )

    async def test_real_httpx_per_request_timeout_bounds_the_whole_stream(self) -> None:
        class SlowDrip(httpx.AsyncByteStream):
            async def __aiter__(self) -> AsyncIterator[bytes]:
                yield b'{"ok":'
                await asyncio.sleep(0.05)
                yield b"true}"

        async def handler(request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={"content-type": "application/json"},
                stream=SlowDrip(),
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN),
                http_client=http,
            )
            with self.assertRaises(XenoteerError) as timed_out:
                await transport.request_with_timeout("GET", "/v1/status", timeout=0.01)
        self.assertEqual(timed_out.exception.code, "request_timeout")

    async def test_artifact_rejects_metadata_before_sink_and_digest_after_prefix(self) -> None:
        body = b"expected"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        writes: list[bytes] = []

        async def write(chunk: bytes) -> None:
            writes.append(chunk)

        def wrong_header(request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": "text/plain",
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=body,
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(wrong_header),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN), http_client=http
            )
            with self.assertRaises(XenoteerError):
                await transport.download_artifact("/v1/artifacts/x", reference, write)
        self.assertEqual(writes, [])

        def wrong_digest(request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": reference.content_type,
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=b"altered!",
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(wrong_digest),
        ) as http:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", TOKEN), http_client=http
            )
            with self.assertRaisesRegex(XenoteerError, "digest"):
                await transport.download_artifact("/v1/artifacts/x", reference, write)
        self.assertEqual(writes, [b"altered!"])


class RuntimeWebSocketTests(unittest.IsolatedAsyncioTestCase):
    async def test_real_websocket_ack_controls_generation_failure_and_close(self) -> None:
        async def handler(socket) -> None:
            hello = json.loads(await socket.recv())
            self.assertEqual(hello["type"], "client.hello")
            self.assertEqual(socket.request.headers["authorization"], f"Bearer {TOKEN}")
            await socket.send(json.dumps(welcome_wire()))
            subscribe = json.loads(await socket.recv())
            await socket.send(
                json.dumps(
                    {
                        "type": "events.subscribed",
                        "request_id": subscribe["request_id"],
                        "topics": subscribe["topics"],
                    }
                )
            )
            await socket.send(
                json.dumps(
                    {
                        "type": "events.replay_complete",
                        "request_id": subscribe["request_id"],
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "through_sequence": "9",
                    }
                )
            )
            await socket.send(
                json.dumps(
                    {
                        "type": "events.resync_required",
                        "request_id": subscribe["request_id"],
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "reason": "generation_changed",
                        "dropped_through": "9",
                        "latest_sequence": "10",
                    }
                )
            )

        async with serve(handler, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            session = await EventSession.connect(
                f"ws://127.0.0.1:{port}",
                f"Bearer {TOKEN}",
                {
                    "type": "client.hello",
                    "request_id": COMMAND_ID,
                    "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
                    "client": {"name": "test", "version": "0"},
                    "resume": None,
                },
                heartbeat_interval=1,
                read_stale_timeout=3,
                max_reconnect_attempts=0,
            )
            ack = await session.subscribe(DESKTOP_ID, GENERATION, ["window.created"])
            self.assertEqual(ack.topics, ("window.created",))
            self.assertIsInstance(await asyncio.wait_for(anext(session), 1), ReplayComplete)
            self.assertIsInstance(await asyncio.wait_for(anext(session), 1), ResyncRequired)
            with self.assertRaises(XenoteerError) as changed:
                await asyncio.wait_for(anext(session), 1)
            self.assertEqual(changed.exception.code, "generation_changed")
            await session.close()

    async def test_real_websocket_permanent_auth_close_is_not_reconnected(self) -> None:
        connections = 0

        async def handler(socket) -> None:
            nonlocal connections
            connections += 1
            await socket.recv()
            await socket.close(4401, "credential rejected")

        async with serve(handler, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            with self.assertRaises(XenoteerError) as auth:
                await EventSession.connect(
                    f"ws://127.0.0.1:{port}",
                    f"Bearer {TOKEN}",
                    {
                        "type": "client.hello",
                        "request_id": COMMAND_ID,
                        "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
                        "client": {"name": "test", "version": "0"},
                        "resume": None,
                    },
                    heartbeat_interval=1,
                    read_stale_timeout=3,
                    max_reconnect_attempts=5,
                )
            self.assertEqual(auth.exception.code, "authentication")
            self.assertEqual(connections, 1)


class LifecycleAndCorpusTests(unittest.IsolatedAsyncioTestCase):
    async def test_server_stale_errors_invalidate_every_direct_handle_path(
        self,
    ) -> None:
        class Transport:
            base_url = "https://xenoteer.test"

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
                del method, path, body, headers
                raise XenoteerError("stale_reference", "server rejected stale handle")

            async def close(self) -> None:
                pass

        transport = Transport()
        desktop = Desktop(transport, DESKTOP_ID, GENERATION, {"major": 1, "minor": 0})
        window_ref = {
            "desktop_id": DESKTOP_ID,
            "desktop_generation": GENERATION,
            "xid": 42,
            "observed_generation": "1",
            "identity_hash": "a" * 64,
        }
        element_ref = {
            "desktop_id": DESKTOP_ID,
            "desktop_generation": GENERATION,
            "atspi_generation": "1",
            "application": {
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "atspi_generation": "1",
                "unique_bus_name": ":1.42",
                "root_object_path": "/root",
                "app_instance_generation": "1",
                "identity_hash": "b" * 64,
            },
            "object_path": "/button",
            "object_identity_hash": "c" * 64,
            "cache_sequence": "1",
        }

        window = Window(desktop, window_ref, reference_token="a" * 32)
        with self.assertRaises(XenoteerError):
            await window.snapshot()
        self.assertTrue(window.stale)

        element = Element(desktop, element_ref)
        with self.assertRaises(XenoteerError):
            await element.snapshot()
        self.assertTrue(element.stale)

        clickable = Element(desktop, element_ref)
        lease = ControlLease(
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
        with self.assertRaises(XenoteerError):
            await clickable.physical_click(lease)
        self.assertTrue(clickable.stale)

    async def test_double_release_and_local_cancellation_are_explicit(self) -> None:
        class Transport:
            base_url = "https://xenoteer.test"

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
                if method == "DELETE":
                    return {
                        "desktop_id": DESKTOP_ID,
                        "desktop_generation": GENERATION,
                        "state": "vacant",
                        "lease_id": None,
                    }
                await asyncio.Future()
                raise AssertionError("unreachable")

            async def close(self) -> None:
                pass

        transport = Transport()
        desktop = Desktop(transport, DESKTOP_ID, GENERATION, {"major": 1, "minor": 0})
        lease = ControlLease(
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
        await lease.release()
        with self.assertRaises(XenoteerError) as released:
            await lease.release()
        self.assertEqual(released.exception.code, "lease_released")

        handle = CommandHandle(
            transport,
            DESKTOP_ID,
            GENERATION,
            {
                "command_id": COMMAND_ID,
                "lifecycle": "accepted",
                "effect_stage": "accepted",
                "accepted_at": "2030-01-01T00:00:00Z",
                "warnings": [],
            },
        )
        task = asyncio.create_task(handle.wait_once(1))
        await asyncio.sleep(0)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertEqual(handle.id, COMMAND_ID)

    async def test_conformance_counts_are_visible_and_failure_free(self) -> None:
        results = await asyncio.to_thread(run_v1_conformance)
        counts = Counter(result.status for result in results)
        self.assertEqual(len(results), 73)
        self.assertEqual(counts, {"passed": 73})


class ConformanceMutationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        root = (
            __import__("pathlib").Path(__file__).resolve().parents[3]
            / "conformance"
            / "v1"
            / "cases"
        )
        cls.cases = [
            case
            for path in sorted(root.glob("*.json"))
            for case in json.loads(path.read_text())["cases"]
        ]

    def fixture(self, operation: str, predicate=lambda case: True):
        return copy.deepcopy(
            next(case for case in self.cases if case["operation"] == operation and predicate(case))
        )

    def assert_mutation_fails(self, case) -> None:
        result = run_cases([case])
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].status, "failed", result[0].detail)

    def test_event_drain_survives_delayed_scheduler_without_replay(self) -> None:
        event_case = self.fixture(
            "event_continuity",
            lambda case: case["id"] == "event.filtered-sequence-jump",
        )
        original_dispatch = EventSession._dispatch
        delayed = False

        async def delayed_dispatch(session, decoded) -> None:
            nonlocal delayed
            if (
                not delayed
                and isinstance(decoded, dict)
                and decoded.get("type") == "event"
                and decoded.get("event", {}).get("sequence") == "15"
            ):
                delayed = True
                await asyncio.sleep(0.15)
            await original_dispatch(session, decoded)

        with mock.patch.object(EventSession, "_dispatch", delayed_dispatch):
            result = run_cases([event_case])

        self.assertTrue(delayed, "the scheduler-delay seam was not exercised")
        self.assertEqual(result[0].status, "passed", result[0].detail)

    def test_event_drain_does_not_use_expected_delivery_count(self) -> None:
        event_case = self.fixture(
            "event_continuity",
            lambda case: case["id"] == "event.filtered-sequence-jump",
        )
        unexpected = copy.deepcopy(event_case["input"]["frames"][-1])
        unexpected["event"]["sequence"] = "16"
        event_case["input"]["frames"].append(unexpected)

        self.assert_mutation_fails(event_case)

    def test_event_completion_barrier_surfaces_terminal_frame(self) -> None:
        event_case = self.fixture(
            "event_continuity",
            lambda case: case["id"] == "event.history-lost",
        )

        result = run_cases([event_case])

        self.assertEqual(result[0].status, "passed", result[0].detail)

    def test_semantic_mutations_fail_the_adapter(self) -> None:
        missing_command = self.fixture("command_reconnect")
        missing_command["input"].pop("command")

        renamed_command = self.fixture("command_reconnect")
        renamed_command["input"]["command"]["type"] = "renamed_probe"

        malformed_event = self.fixture("event_continuity")
        malformed_event["input"]["frames"][0] = {
            "type": "event",
            "request_id": malformed_event["input"]["subscription_request_id"],
        }

        wrong_secret = self.fixture("redaction")
        wrong_secret["input"]["secret"] = "XENOTEER_WRONG_RAW_SECRET"

        reversed_sequence = self.fixture(
            "event_continuity",
            lambda case: (
                len(case["input"]["frames"]) >= 2
                and all(frame.get("type") == "event" for frame in case["input"]["frames"][:2])
                and case["input"]["frames"][0]["event"]["sequence"]
                != case["input"]["frames"][1]["event"]["sequence"]
            ),
        )
        reversed_sequence["input"]["frames"][:2] = reversed(
            reversed_sequence["input"]["frames"][:2]
        )

        wrong_outcome = self.fixture("classify_terminal_effect")
        wrong_outcome["expect"]["outcome_type"] = "wrong_outcome"

        missing_preserve = self.fixture("decode_event")
        missing_preserve["expect"]["preserve"].pop()

        for mutated in (
            missing_command,
            renamed_command,
            malformed_event,
            wrong_secret,
            reversed_sequence,
            wrong_outcome,
            missing_preserve,
        ):
            with self.subTest(mutation=mutated["operation"]):
                self.assert_mutation_fails(mutated)

    def test_generation_change_executes_the_public_submission_fence(self) -> None:
        generation_case = self.fixture(
            "command_reconnect",
            lambda case: case["id"] == "command.reconnect.generation-changed",
        )
        observed_checks = 0
        original = GenerationRegistry.require_current

        def observed(registry: GenerationRegistry) -> None:
            nonlocal observed_checks
            observed_checks += 1
            original(registry)

        with mock.patch.object(
            GenerationRegistry,
            "require_current",
            observed,
        ):
            results = run_cases([generation_case])
        self.assertEqual(results[0].status, "passed", results[0].detail)
        self.assertGreaterEqual(
            observed_checks,
            2,
            "generation reconnect case never attempted a fenced SDK send",
        )

    def test_direct_adapter_rejects_wrong_frozen_corpus_identity(self) -> None:
        script_path = pathlib.Path(__file__).resolve().parents[1] / "scripts" / "run_conformance.py"
        spec = importlib.util.spec_from_file_location(
            "xenoteer_python_conformance_adapter",
            script_path,
        )
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader if spec is not None else None)
        assert spec is not None and spec.loader is not None
        adapter = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(adapter)

        base = {
            "adapter_protocol": 1,
            "cases": [],
            "corpus": "xenoteer-conformance-v1",
            "corpus_sha256": ("6cc98e72e1de6591cce2d0661f4fc3ea508535d310a40746aa3ad8bd1e61e7fc"),
            "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
        }
        mutations = (
            {**base, "corpus": "xenoteer-conformance-v2"},
            {**base, "corpus_sha256": "0" * 64},
            {**base, "protocol": {"major": 1, "min_minor": 0, "max_minor": 1}},
        )
        for payload in mutations:
            with self.subTest(payload=payload):
                diagnostic = io.StringIO()
                with (
                    mock.patch("sys.stdin", io.StringIO(json.dumps(payload))),
                    redirect_stderr(diagnostic),
                ):
                    self.assertEqual(adapter.main(), 2)
                self.assertIn("unsupported", diagnostic.getvalue())


if __name__ == "__main__":
    unittest.main()
