# SPDX-License-Identifier: Apache-2.0
"""Connection ownership, reconnect-policy, and safe-log contract tests."""

from __future__ import annotations

import asyncio
import gc
import hashlib
import inspect
import json
import time
import unittest
import weakref
from collections.abc import AsyncIterator, Callable, Mapping
from dataclasses import FrozenInstanceError
from types import SimpleNamespace
from typing import Any
from unittest import mock

import httpx
import xenoteer.events as events_module

from xenoteer import (
    ArtifactRef,
    Artifacts,
    ClientOptions,
    EventSession,
    HttpTransport,
    ReconnectPolicy,
    SafeLogEvent,
    WebSocketFactory,
    WebSocketLike,
    XenoteerClient,
    XenoteerError,
    classify_safe_route,
)


DESKTOP_ID = "10000000-0000-4000-8000-000000000001"
GENERATION = "10000000-0000-4000-8000-000000000002"
ARTIFACT_ID = "10000000-0000-4000-8000-000000000003"
TOKEN_CANARY = "TOKEN_CANARY_NEVER_LOG_"
PATH_CANARY = "path-canary-never-log"


def status_wire() -> dict[str, Any]:
    return {
        "server_version": "0.1.0",
        "protocol_min": {"major": 1, "minor": 0},
        "protocol_max": {"major": 1, "minor": 0},
        "server_time": "2026-07-30T00:00:00Z",
        "desktop": {
            "id": DESKTOP_ID,
            "generation": GENERATION,
            "state": "ready",
            "reason_code": None,
        },
        "capabilities": {"capabilities": []},
    }


def welcome_wire() -> str:
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
                "heartbeat_ms": 300_000,
                "normal_outbound_capacity": 32,
                "reserved_outbound_capacity": 8,
                "max_command_watches": 16,
            },
            "resume": {"status": "not_requested"},
        }
    )


def hello_wire() -> dict[str, Any]:
    return {
        "type": "client.hello",
        "request_id": "10000000-0000-4000-8000-000000000010",
        "client": {"name": "test", "version": "1"},
        "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
        "resume": None,
    }


def artifact_wire(body: bytes, *, purpose: str = "screenshot") -> dict[str, Any]:
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


class Socket:
    def __init__(self, received: list[object]) -> None:
        self.received: asyncio.Queue[object] = asyncio.Queue()
        for item in received:
            self.received.put_nowait(item)
        self.sent: list[dict[str, Any]] = []
        self.close_calls = 0

    async def send(self, message: str) -> None:
        self.sent.append(json.loads(message))

    async def recv(self) -> object:
        item = await self.received.get()
        if isinstance(item, BaseException):
            raise item
        return item

    async def close(self, **kwargs: Any) -> None:
        del kwargs
        self.close_calls += 1


class Transport:
    def __init__(
        self,
        responder: Callable[[str, str], dict[str, Any]],
        *,
        tokens: list[str] | None = None,
    ) -> None:
        self._responder = responder
        self._tokens = tokens or ["t" * 48]
        self.request_calls: list[tuple[str, str]] = []
        self.authorization_calls = 0
        self.close_calls = 0

    @property
    def base_url(self) -> str:
        return "https://xenoteer.test"

    async def authorization_header(self) -> str:
        token = self._tokens[self.authorization_calls % len(self._tokens)]
        self.authorization_calls += 1
        return f"Bearer {token}"

    async def request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
    ) -> dict[str, Any]:
        del body, headers
        self.request_calls.append((method, path))
        return self._responder(method, path)

    async def close(self) -> None:
        self.close_calls += 1


class PublicContractTests(unittest.TestCase):
    def test_public_connection_types_and_frozen_reconnect_policy(self) -> None:
        policy = ReconnectPolicy(
            max_attempts=7,
            initial_delay=0.25,
            max_delay=9.0,
            jitter_min=0.2,
            jitter_max=1.2,
        )
        self.assertEqual(policy.max_attempts, 7)
        with self.assertRaises(FrozenInstanceError):
            policy.max_attempts = 1  # type: ignore[misc]

        event = SafeLogEvent(
            operation="http.request",
            outcome="started",
            method="GET",
            route="/v1/status",
        )
        with self.assertRaises(FrozenInstanceError):
            event.route = "unknown"  # type: ignore[misc]

        socket: WebSocketLike = Socket([welcome_wire()])

        def factory(*_args: Any, **_kwargs: Any) -> WebSocketLike:
            return socket

        checked_factory: WebSocketFactory = factory
        self.assertIsNotNone(checked_factory)

    def test_reconnect_policy_rejects_every_invalid_bound(self) -> None:
        invalid = (
            {"max_attempts": -1},
            {"max_attempts": 21},
            {"max_attempts": True},
            {"initial_delay": -0.1},
            {"max_delay": 61.0},
            {"initial_delay": 2.0, "max_delay": 1.0},
            {"jitter_min": -0.1},
            {"jitter_max": 2.1},
            {"jitter_min": 1.1, "jitter_max": 1.0},
        )
        for values in invalid:
            with self.subTest(values=values), self.assertRaises(XenoteerError):
                ReconnectPolicy(**values)

    def test_connect_timeout_rejects_every_invalid_bound(self) -> None:
        for value in (
            0,
            -1,
            60.000_001,
            float("inf"),
            float("nan"),
            True,
            "10",
        ):
            with self.subTest(value=value), self.assertRaises(XenoteerError):
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    connect_timeout=value,  # type: ignore[arg-type]
                )

    def test_async_safe_log_hooks_are_rejected_before_io(self) -> None:
        async def asynchronous_hook(event: SafeLogEvent) -> None:
            del event

        with self.assertRaisesRegex(XenoteerError, "synchronous"):
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                safe_log_hook=asynchronous_hook,
            )

    def test_client_metadata_rejects_unicode_control_characters(self) -> None:
        for field, value in (
            ("client_name", "agent\nforged"),
            ("client_name", "agent\u0085forged"),
            ("client_version", "1.0\x00forged"),
            ("client_version", "1.0\u009fforged"),
        ):
            with self.subTest(field=field, value=value):
                with self.assertRaisesRegex(XenoteerError, field.replace("_", " ")):
                    ClientOptions(
                        "https://xenoteer.test",
                        "t" * 48,
                        **{field: value},
                    )

    def test_route_classifier_never_returns_raw_identifiers_or_queries(self) -> None:
        cases = {
            "/v1/status": "/v1/status",
            "/v1/capabilities": "/v1/capabilities",
            f"/v1/artifacts/{PATH_CANARY}?desktop_id={DESKTOP_ID}": (
                "/v1/artifacts/{artifact_id}"
            ),
            f"/v1/desktops/{DESKTOP_ID}/commands": (
                "/v1/desktops/{desktop_id}/commands"
            ),
            f"/v1/desktops/{DESKTOP_ID}/commands/{PATH_CANARY}?wait_ms=1": (
                "/v1/desktops/{desktop_id}/commands/{command_id}"
            ),
            f"/private/{PATH_CANARY}?token={TOKEN_CANARY}": "unknown",
        }
        for path, expected in cases.items():
            with self.subTest(path=path):
                route = classify_safe_route(path)
                self.assertEqual(route, expected)
                self.assertNotIn(PATH_CANARY, route)
                self.assertNotIn(TOKEN_CANARY, route)


class OwnershipTests(unittest.IsolatedAsyncioTestCase):
    async def test_internal_transport_closes_on_success_failure_and_cancellation(
        self,
    ) -> None:
        successful = Transport(lambda _method, _path: status_wire())
        with mock.patch(
            "xenoteer.client.HttpTransport", return_value=successful
        ):
            client = await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48)
            )
        await client.close()
        await client.close()
        self.assertEqual(successful.close_calls, 1)

        malformed = Transport(lambda _method, _path: {"malformed": True})
        with (
            mock.patch("xenoteer.client.HttpTransport", return_value=malformed),
            self.assertRaises(XenoteerError),
        ):
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48)
            )
        self.assertEqual(malformed.close_calls, 1)

        started = asyncio.Event()
        never = asyncio.Event()

        class BlockingTransport(Transport):
            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del method, path, body, headers
                started.set()
                await never.wait()
                return status_wire()

        cancelled = BlockingTransport(lambda _method, _path: status_wire())
        with mock.patch(
            "xenoteer.client.HttpTransport", return_value=cancelled
        ):
            task = asyncio.create_task(
                XenoteerClient.connect(
                    ClientOptions("https://xenoteer.test", "t" * 48)
                )
            )
            await asyncio.wait_for(started.wait(), 1)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(cancelled.close_calls, 1)

    async def test_injected_transport_is_borrowed_by_default(self) -> None:
        transport = Transport(lambda _method, _path: status_wire())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", "t" * 48),
            transport=transport,
            websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
        )
        await client.close()
        self.assertEqual(transport.close_calls, 0)

    async def test_client_owned_transport_closes_on_failure_and_success(self) -> None:
        successful = Transport(lambda _method, _path: status_wire())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", "t" * 48),
            transport=successful,
            transport_ownership="client",
            websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
        )
        await client.close()
        await client.close()
        self.assertEqual(successful.close_calls, 1)

        failing = Transport(lambda _method, _path: {"malformed": True})
        with self.assertRaises(XenoteerError):
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48),
                transport=failing,
                transport_ownership="client",
                websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
            )
        self.assertEqual(failing.close_calls, 1)

    async def test_failed_connect_bounds_cooperative_owned_transport_cleanup(
        self,
    ) -> None:
        close_started = asyncio.Event()
        close_cancelled = asyncio.Event()

        class HangingCloseTransport(Transport):
            async def close(self) -> None:
                self.close_calls += 1
                close_started.set()
                try:
                    await asyncio.Event().wait()
                finally:
                    close_cancelled.set()

        transport = HangingCloseTransport(
            lambda _method, _path: {"malformed": True}
        )
        with self.assertRaises(XenoteerError) as caught:
            await asyncio.wait_for(
                XenoteerClient.connect(
                    ClientOptions(
                        "https://xenoteer.test",
                        "t" * 48,
                        request_timeout=0.01,
                    ),
                    transport=transport,
                    transport_ownership="client",
                    websocket_factory=lambda *_args, **_kwargs: Socket(
                        [welcome_wire()]
                    ),
                ),
                0.1,
            )
        self.assertEqual(caught.exception.code, "invalid_response")
        self.assertEqual(transport.close_calls, 1)
        self.assertTrue(close_started.is_set())
        self.assertTrue(close_cancelled.is_set())
        await asyncio.sleep(0)
        self.assertFalse(
            any(
                task.get_name() == "xenoteer-failed-connect-close"
                and not task.done()
                for task in asyncio.all_tasks()
            )
        )

    async def test_failed_connect_preserves_original_error_when_owned_close_fails(
        self,
    ) -> None:
        incompatible = status_wire()
        incompatible["protocol_min"] = {"major": 2, "minor": 0}
        incompatible["protocol_max"] = {"major": 2, "minor": 0}

        class ThrowingCloseTransport(Transport):
            async def close(self) -> None:
                self.close_calls += 1
                raise RuntimeError("cleanup failure must not replace negotiation")

        transport = ThrowingCloseTransport(
            lambda _method, _path: incompatible
        )
        with self.assertRaises(XenoteerError) as caught:
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48),
                transport=transport,
                transport_ownership="client",
                websocket_factory=lambda *_args, **_kwargs: Socket(
                    [welcome_wire()]
                ),
            )
        self.assertEqual(caught.exception.code, "unsupported_major")
        self.assertEqual(transport.close_calls, 1)

    async def test_cancelled_connect_bounds_cooperative_owned_transport_cleanup(
        self,
    ) -> None:
        request_started = asyncio.Event()
        request_cancelled = asyncio.Event()
        close_started = asyncio.Event()
        close_cancelled = asyncio.Event()
        release_close = asyncio.Event()

        class CancellationTransport(Transport):
            async def request(
                self,
                method: str,
                path: str,
                body: Mapping[str, Any] | None = None,
                *,
                headers: Mapping[str, str] | None = None,
            ) -> dict[str, Any]:
                del method, path, body, headers
                request_started.set()
                try:
                    await asyncio.Event().wait()
                finally:
                    request_cancelled.set()
                return status_wire()

            async def close(self) -> None:
                self.close_calls += 1
                close_started.set()
                try:
                    await release_close.wait()
                finally:
                    close_cancelled.set()

        transport = CancellationTransport(
            lambda _method, _path: status_wire()
        )
        operation = asyncio.create_task(
            XenoteerClient.connect(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    connect_timeout=60,
                    request_timeout=0.01,
                ),
                transport=transport,
                transport_ownership="client",
                websocket_factory=lambda *_args, **_kwargs: Socket(
                    [welcome_wire()]
                ),
            )
        )
        await asyncio.wait_for(request_started.wait(), 0.1)
        operation.cancel()
        try:
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(asyncio.shield(operation), 0.1)
        finally:
            release_close.set()
            if not operation.done():
                with self.assertRaises(asyncio.CancelledError):
                    await asyncio.wait_for(operation, 0.1)
        self.assertTrue(request_cancelled.is_set())
        self.assertTrue(close_started.is_set())
        self.assertTrue(close_cancelled.is_set())
        self.assertEqual(transport.close_calls, 1)
        await asyncio.sleep(0)
        self.assertFalse(
            any(
                task.get_name() == "xenoteer-failed-connect-close"
                and not task.done()
                for task in asyncio.all_tasks()
            )
        )

    async def test_borrowed_transport_survives_connect_failure(self) -> None:
        class BorrowedTransport(Transport):
            async def close(self) -> None:
                self.close_calls += 1
                raise AssertionError("borrowed transport must not be closed")

        transport = BorrowedTransport(
            lambda _method, _path: {"malformed": True}
        )
        with self.assertRaises(XenoteerError):
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48),
                transport=transport,
                websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
            )
        self.assertEqual(transport.close_calls, 0)

    async def test_injected_httpx_client_is_always_borrowed(self) -> None:
        async def handler(request: httpx.Request) -> httpx.Response:
            del request
            return httpx.Response(200, json=status_wire())

        http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        )
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", "t" * 48),
            http_client=http_client,
            websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
        )
        await client.close()
        self.assertFalse(http_client.is_closed)
        await http_client.aclose()

        failing_http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(
                lambda _request: httpx.Response(200, json={"malformed": True})
            ),
        )
        with self.assertRaises(XenoteerError):
            await XenoteerClient.connect(
                ClientOptions("https://xenoteer.test", "t" * 48),
                http_client=failing_http_client,
            )
        self.assertFalse(failing_http_client.is_closed)
        await failing_http_client.aclose()

    async def test_adapter_conflicts_and_ownership_are_rejected_before_io(self) -> None:
        transport = Transport(lambda _method, _path: status_wire())
        async with httpx.AsyncClient() as http_client:
            with self.assertRaisesRegex(XenoteerError, "simultaneously"):
                await XenoteerClient.connect(
                    ClientOptions("https://xenoteer.test", "t" * 48),
                    http_client=http_client,
                    transport=transport,
                )
            with self.assertRaisesRegex(XenoteerError, "borrowed"):
                await XenoteerClient.connect(
                    ClientOptions("https://xenoteer.test", "t" * 48),
                    http_client=http_client,
                    transport_ownership="client",
                )
        self.assertEqual(transport.request_calls, [])

    async def test_custom_http_adapter_requires_paired_websocket_factory(self) -> None:
        transport = Transport(lambda _method, _path: status_wire())
        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", "t" * 48),
            transport=transport,
        )
        with self.assertRaisesRegex(XenoteerError, "paired WebSocket factory"):
            await client.open_events()
        await client.close()


class SafeLogTests(unittest.IsolatedAsyncioTestCase):
    async def test_http_and_artifact_attempts_have_exact_safe_log_pairs(self) -> None:
        body = b"safe artifact body"
        events: list[SafeLogEvent] = []
        tokens = [f"{letter}{TOKEN_CANARY}{'x' * 24}" for letter in "abcdef"]
        used_headers: list[str] = []

        async def token_provider() -> str:
            return tokens.pop(0)

        async def handler(request: httpx.Request) -> httpx.Response:
            used_headers.append(request.headers["authorization"])
            if request.url.path == "/v1/status":
                return httpx.Response(200, json=status_wire())
            if request.url.path == "/v1/capabilities":
                return httpx.Response(200, json={"capabilities": []})
            if request.method == "POST":
                return httpx.Response(
                    201,
                    json=artifact_wire(body, purpose="clipboard_input"),
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
        ) as http_client:
            client = await XenoteerClient.connect(
                ClientOptions(
                    "https://xenoteer.test",
                    token_provider,
                    safe_log_hook=events.append,
                ),
                http_client=http_client,
                websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
            )
            await client.capabilities()
            artifact = ArtifactRef.from_wire(artifact_wire(body))
            await client.desktop().artifacts.upload_clipboard_input(
                "application/octet-stream", body
            )
            self.assertEqual(
                await client.desktop().artifacts.download_bytes(artifact),
                body,
            )
            await client.desktop().artifacts.delete(artifact)
            await client.close()

        self.assertEqual(
            [event.operation for event in events[::2]],
            [
                "http.request",
                "http.request",
                "artifact.upload",
                "artifact.download",
                "artifact.delete",
            ],
        )
        self.assertTrue(all(event.outcome == "started" for event in events[::2]))
        self.assertTrue(all(event.outcome == "succeeded" for event in events[1::2]))
        self.assertEqual(len(events), 10)
        self.assertEqual(len(set(used_headers)), 5)
        rendered = repr(events)
        self.assertNotIn(TOKEN_CANARY, rendered)
        self.assertNotIn(ARTIFACT_ID, rendered)
        self.assertNotIn(DESKTOP_ID, rendered)
        self.assertNotIn(body.decode(), rendered)

    async def test_provider_status_sink_and_hook_failures_preserve_safe_results(self) -> None:
        emitted: list[SafeLogEvent] = []

        async def failed_provider() -> str:
            raise RuntimeError(f"{TOKEN_CANARY}{PATH_CANARY}")

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(
                lambda _request: httpx.Response(200, json=status_wire())
            ),
        ) as http_client:
            with self.assertRaises(XenoteerError) as provider_error:
                await XenoteerClient.connect(
                    ClientOptions(
                        "https://xenoteer.test",
                        failed_provider,
                        safe_log_hook=emitted.append,
                    ),
                    http_client=http_client,
                )
        self.assertEqual(provider_error.exception.code, "invalid_token")
        self.assertEqual([event.outcome for event in emitted], ["started", "failed"])
        self.assertEqual(emitted[-1].error_code, "invalid_token")
        self.assertNotIn(TOKEN_CANARY, repr(emitted))

        calls = 0

        def throwing_hook(event: SafeLogEvent) -> None:
            nonlocal calls
            del event
            calls += 1
            raise RuntimeError(f"{TOKEN_CANARY}{PATH_CANARY}")

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(
                lambda _request: httpx.Response(
                    503,
                    headers={"content-type": "application/json"},
                    json={"untrusted": f"{TOKEN_CANARY}{PATH_CANARY}"},
                )
            ),
        ) as http_client:
            with self.assertRaises(XenoteerError) as status_error:
                await XenoteerClient.connect(
                    ClientOptions(
                        "https://xenoteer.test",
                        "t" * 48,
                        safe_log_hook=throwing_hook,
                    ),
                    http_client=http_client,
                )
        self.assertEqual(status_error.exception.status, 503)
        self.assertEqual(calls, 2)

        body = b"verified"
        sink_events: list[SafeLogEvent] = []

        async def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": "application/octet-stream",
                    "content-length": str(len(body)),
                    "x-content-sha256": hashlib.sha256(body).hexdigest(),
                },
                content=body,
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            from xenoteer import HttpTransport

            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    safe_log_hook=sink_events.append,
                ),
                http_client=http_client,
            )

            async def broken_sink(_chunk: bytes) -> None:
                raise RuntimeError(f"{TOKEN_CANARY}{PATH_CANARY}")

            with self.assertRaises(XenoteerError) as sink_error:
                await transport.download_artifact(
                    f"/v1/artifacts/{ARTIFACT_ID}?desktop_id={DESKTOP_ID}",
                    ArtifactRef.from_wire(artifact_wire(body)),
                    broken_sink,
                )
        self.assertEqual(sink_error.exception.code, "transport")
        self.assertEqual(
            [event.outcome for event in sink_events],
            ["started", "failed"],
        )
        self.assertNotIn(PATH_CANARY, repr(sink_events))

    async def test_every_http_failure_path_gets_one_failed_terminal_event(self) -> None:
        body = b"bounded"
        events: list[SafeLogEvent] = []

        async def handler(request: httpx.Request) -> httpx.Response:
            if request.method == "POST":
                return httpx.Response(
                    201,
                    json={
                        **artifact_wire(body, purpose="clipboard_input"),
                        "sha256": "0" * 64,
                    },
                )
            return httpx.Response(
                500,
                headers={"content-type": "application/json"},
                json={"server_prose": f"{TOKEN_CANARY}{PATH_CANARY}"},
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            from xenoteer import HttpTransport

            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    safe_log_hook=events.append,
                ),
                http_client=http_client,
            )
            with self.assertRaises(XenoteerError) as json_failure:
                await transport.request(
                    "POST",
                    "/v1/status",
                    {"not_json": object()},
                )
            self.assertEqual(json_failure.exception.code, "invalid_request")

            with self.assertRaises(XenoteerError) as upload_failure:
                await transport.upload_artifact(
                    "/v1/artifacts?purpose=clipboard_input",
                    "application/octet-stream",
                    body,
                )
            self.assertEqual(upload_failure.exception.code, "invalid_response")

            with self.assertRaises(XenoteerError) as delete_failure:
                await transport.delete_artifact(
                    f"/v1/artifacts/{ARTIFACT_ID}?desktop_id={DESKTOP_ID}"
                )
            self.assertEqual(delete_failure.exception.status, 500)

        self.assertEqual(
            [
                (event.operation, event.outcome, event.error_code)
                for event in events
            ],
            [
                ("http.request", "started", None),
                ("http.request", "failed", "invalid_request"),
                ("artifact.upload", "started", None),
                ("artifact.upload", "failed", "invalid_response"),
                ("artifact.delete", "started", None),
                ("artifact.delete", "failed", "unexpected_http_status"),
            ],
        )
        self.assertNotIn(TOKEN_CANARY, repr(events))
        self.assertNotIn(PATH_CANARY, repr(events))
        self.assertNotIn(ARTIFACT_ID, repr(events))

    async def test_cancelled_download_logs_failed_only_after_stream_abandonment(
        self,
    ) -> None:
        body = b"cancelled body"
        started = asyncio.Event()
        never = asyncio.Event()
        events: list[SafeLogEvent] = []

        class BlockingStream(httpx.AsyncByteStream):
            async def __aiter__(self) -> AsyncIterator[bytes]:
                started.set()
                await never.wait()
                yield body

        async def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": "application/octet-stream",
                    "content-length": str(len(body)),
                    "x-content-sha256": hashlib.sha256(body).hexdigest(),
                },
                stream=BlockingStream(),
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            from xenoteer import HttpTransport

            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    safe_log_hook=events.append,
                ),
                http_client=http_client,
            )

            async def discard(_chunk: bytes) -> None:
                return None

            task = asyncio.create_task(
                transport.download_artifact(
                    f"/v1/artifacts/{ARTIFACT_ID}?desktop_id={DESKTOP_ID}",
                    ArtifactRef.from_wire(artifact_wire(body)),
                    discard,
                )
            )
            await asyncio.wait_for(started.wait(), 1)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task

        self.assertEqual(
            [(event.outcome, event.error_code) for event in events],
            [("started", None), ("failed", "cancelled")],
        )

    async def test_custom_transport_is_logged_without_retries_or_error_replacement(
        self,
    ) -> None:
        events: list[SafeLogEvent] = []
        failure = XenoteerError("resource_exhausted", "original outcome")

        def responder(_method: str, path: str) -> dict[str, Any]:
            if path == "/v1/status":
                return status_wire()
            raise failure

        transport = Transport(responder)
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                safe_log_hook=events.append,
            ),
            transport=transport,
            websocket_factory=lambda *_args, **_kwargs: Socket([welcome_wire()]),
        )
        with self.assertRaises(XenoteerError) as caught:
            await client.capabilities()
        self.assertIs(caught.exception, failure)
        self.assertEqual(
            transport.request_calls,
            [("GET", "/v1/status"), ("GET", "/v1/capabilities")],
        )
        self.assertEqual(
            [(event.outcome, event.error_code) for event in events],
            [
                ("started", None),
                ("succeeded", None),
                ("started", None),
                ("failed", "resource_exhausted"),
            ],
        )
        await client.close()

    async def test_throwing_hook_never_retries_or_replaces_a_mutation_failure(
        self,
    ) -> None:
        hook_calls = 0
        mutation_calls = 0

        def throwing_hook(_event: SafeLogEvent) -> None:
            nonlocal hook_calls
            hook_calls += 1
            raise RuntimeError(f"{TOKEN_CANARY}{PATH_CANARY}")

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal mutation_calls
            mutation_calls += 1
            return httpx.Response(
                409,
                headers={"content-type": "application/json"},
                json={"server_prose": f"{TOKEN_CANARY}{PATH_CANARY}"},
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            from xenoteer import HttpTransport

            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    safe_log_hook=throwing_hook,
                ),
                http_client=http_client,
            )
            with self.assertRaises(XenoteerError) as caught:
                await transport.request(
                    "POST",
                    f"/v1/desktops/{DESKTOP_ID}/commands",
                    {"command_id": PATH_CANARY},
                )
        self.assertEqual(caught.exception.status, 409)
        self.assertEqual(mutation_calls, 1)
        self.assertEqual(hook_calls, 2)
        self.assertNotIn(TOKEN_CANARY, repr(caught.exception))
        self.assertNotIn(PATH_CANARY, repr(caught.exception))

    async def test_safe_hook_does_not_catch_base_exception(self) -> None:
        transport = Transport(lambda _method, _path: status_wire())

        def interrupting_hook(_event: SafeLogEvent) -> None:
            raise KeyboardInterrupt

        with self.assertRaises(KeyboardInterrupt):
            await XenoteerClient.connect(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    safe_log_hook=interrupting_hook,
                ),
                transport=transport,
                websocket_factory=lambda *_args, **_kwargs: Socket(
                    [welcome_wire()]
                ),
            )
        self.assertEqual(transport.request_calls, [])

    async def test_http_token_provider_is_bounded_before_adapter_io(self) -> None:
        provider_started = asyncio.Event()
        never = asyncio.Event()
        adapter_calls = 0
        events: list[SafeLogEvent] = []

        async def provider() -> str:
            provider_started.set()
            await never.wait()
            return "t" * 48

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal adapter_calls
            adapter_calls += 1
            return httpx.Response(200, json=status_wire())

        http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        )
        with self.assertRaises(XenoteerError) as caught:
            await XenoteerClient.connect(
                ClientOptions(
                    "https://xenoteer.test",
                    provider,
                    request_timeout=0.01,
                    safe_log_hook=events.append,
                ),
                http_client=http_client,
            )
        self.assertTrue(provider_started.is_set())
        self.assertEqual(caught.exception.code, "request_timeout")
        self.assertEqual(adapter_calls, 0)
        self.assertFalse(http_client.is_closed)
        self.assertEqual(
            [(event.outcome, event.error_code) for event in events],
            [("started", None), ("failed", "request_timeout")],
        )
        await http_client.aclose()

    async def test_sync_token_provider_is_rejected_without_invocation(self) -> None:
        provider_calls = 0

        def blocking_sync_provider() -> str:
            nonlocal provider_calls
            provider_calls += 1
            return "s" * 48

        with self.assertRaises(XenoteerError) as caught:
            ClientOptions(
                "https://xenoteer.test",
                blocking_sync_provider,
                request_timeout=0.01,
            )
        self.assertEqual(caught.exception.code, "invalid_request")
        self.assertEqual(provider_calls, 0)

    async def test_repeated_async_provider_timeouts_cancel_and_cleanup(self) -> None:
        active_providers = 0
        cancellations = 0
        completions = 0
        adapter_calls = 0
        events: list[SafeLogEvent] = []

        async def provider() -> str:
            nonlocal active_providers, cancellations, completions
            active_providers += 1
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cancellations += 1
                raise
            finally:
                active_providers -= 1
                completions += 1

        def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal adapter_calls
            adapter_calls += 1
            return httpx.Response(200, json=status_wire())

        http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        )
        baseline_tasks = set(asyncio.all_tasks())
        for attempt in range(5):
            with self.assertRaises(XenoteerError) as caught:
                await XenoteerClient.connect(
                    ClientOptions(
                        "https://xenoteer.test",
                        provider,
                        request_timeout=0.005,
                        safe_log_hook=events.append,
                    ),
                    http_client=http_client,
                )
            self.assertEqual(caught.exception.code, "request_timeout")
            self.assertEqual(active_providers, 0, f"attempt {attempt} leaked provider")
            self.assertFalse(http_client.is_closed)

        await asyncio.sleep(0)
        self.assertEqual(cancellations, 5)
        self.assertEqual(completions, 5)
        self.assertEqual(active_providers, 0)
        self.assertEqual(adapter_calls, 0)
        self.assertEqual(
            [(event.outcome, event.error_code) for event in events],
            [("started", None), ("failed", "request_timeout")] * 5,
        )
        self.assertEqual(set(asyncio.all_tasks()), baseline_tasks)
        await http_client.aclose()


class AbsoluteHttpDeadlineTests(unittest.IsolatedAsyncioTestCase):
    async def test_one_budget_spans_auth_and_every_http_operation(self) -> None:
        body = b"deadline-body"
        reference = ArtifactRef.from_wire(artifact_wire(body))

        for operation in ("json", "upload", "download", "delete"):
            with self.subTest(operation=operation):
                events: list[SafeLogEvent] = []

                async def provider() -> str:
                    await asyncio.sleep(0.025)
                    return "t" * 48

                class SlowBody(httpx.AsyncByteStream):
                    async def __aiter__(self) -> AsyncIterator[bytes]:
                        await asyncio.sleep(0.03)
                        yield body

                async def handler(request: httpx.Request) -> httpx.Response:
                    if operation == "download":
                        return httpx.Response(
                            200,
                            headers={
                                "content-type": reference.content_type,
                                "content-length": str(len(body)),
                                "x-content-sha256": reference.sha256,
                            },
                            stream=SlowBody(),
                        )
                    if operation == "upload":
                        await request.aread()
                        await asyncio.sleep(0.03)
                        return httpx.Response(
                            201,
                            headers={"content-type": "application/json"},
                            json=artifact_wire(body, purpose="clipboard_input"),
                        )
                    await asyncio.sleep(0.03)
                    if operation == "delete":
                        return httpx.Response(
                            204, headers={"content-length": "0"}
                        )
                    return httpx.Response(
                        200,
                        headers={"content-type": "application/json"},
                        json={"ok": True},
                    )

                http_client = httpx.AsyncClient(
                    base_url="https://xenoteer.test",
                    transport=httpx.MockTransport(handler),
                )
                transport = HttpTransport(
                    ClientOptions(
                        "https://xenoteer.test",
                        provider,
                        request_timeout=0.04,
                        safe_log_hook=events.append,
                    ),
                    http_client=http_client,
                )

                async def exercise() -> object:
                    if operation == "json":
                        return await transport.request("GET", "/v1/status")
                    if operation == "upload":
                        return await transport.upload_artifact(
                            "/v1/artifacts?purpose=clipboard_input",
                            "application/octet-stream",
                            body,
                        )
                    if operation == "download":
                        async def discard(_chunk: bytes) -> None:
                            return None

                        return await transport.download_artifact(
                            f"/v1/artifacts/{ARTIFACT_ID}",
                            reference,
                            discard,
                        )
                    return await transport.delete_artifact(
                        f"/v1/artifacts/{ARTIFACT_ID}"
                    )

                with self.assertRaises(XenoteerError) as caught:
                    await asyncio.wait_for(exercise(), 0.2)
                self.assertEqual(caught.exception.code, "request_timeout")
                self.assertEqual(
                    [(event.outcome, event.error_code) for event in events],
                    [("started", None), ("failed", "request_timeout")],
                )
                await http_client.aclose()

    async def test_client_close_cancels_hanging_provider_and_operation(self) -> None:
        provider_started = asyncio.Event()
        provider_cancelled = asyncio.Event()
        provider_calls = 0

        async def provider() -> str:
            nonlocal provider_calls
            provider_calls += 1
            if provider_calls == 1:
                return "t" * 48
            provider_started.set()
            try:
                await asyncio.Event().wait()
            finally:
                provider_cancelled.set()

        def handler(request: httpx.Request) -> httpx.Response:
            wire = (
                status_wire()
                if request.url.path == "/v1/status"
                else {"capabilities": []}
            )
            return httpx.Response(
                200,
                headers={"content-type": "application/json"},
                json=wire,
            )

        http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        )
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                provider,
                request_timeout=60,
            ),
            http_client=http_client,
        )
        operation = asyncio.create_task(client.capabilities())
        await asyncio.wait_for(provider_started.wait(), 0.1)
        await asyncio.wait_for(client.close(), 0.1)
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(operation, 0.1)
        self.assertTrue(provider_cancelled.is_set())
        self.assertFalse(http_client.is_closed)
        await http_client.aclose()

    async def test_body_stall_times_out_and_closes_response_stream(self) -> None:
        stream_cancelled = asyncio.Event()
        stream_closed = asyncio.Event()

        class StalledBody(httpx.AsyncByteStream):
            async def __aiter__(self) -> AsyncIterator[bytes]:
                yield b'{"ok":'
                try:
                    await asyncio.Event().wait()
                finally:
                    stream_cancelled.set()

            async def aclose(self) -> None:
                stream_closed.set()

        async def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={"content-type": "application/json"},
                stream=StalledBody(),
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    request_timeout=0.01,
                ),
                http_client=http_client,
            )
            with self.assertRaises(XenoteerError) as caught:
                await asyncio.wait_for(
                    transport.request("GET", "/v1/status"), 0.1
                )
        self.assertEqual(caught.exception.code, "request_timeout")
        self.assertTrue(stream_cancelled.is_set())
        self.assertTrue(stream_closed.is_set())


class ArtifactSinkPolicyTests(unittest.IsolatedAsyncioTestCase):
    async def test_sync_sinks_are_rejected_before_invocation_or_http_io(
        self,
    ) -> None:
        body = b"artifact sink policy"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        adapter_calls = 0
        sink_calls = 0

        async def handler(_request: httpx.Request) -> httpx.Response:
            nonlocal adapter_calls
            adapter_calls += 1
            return httpx.Response(
                200,
                headers={
                    "content-type": reference.content_type,
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=body,
            )

        def blocking_sink(_chunk: bytes) -> None:
            nonlocal sink_calls
            sink_calls += 1
            time.sleep(0.08)

        class BlockingSink:
            def __call__(self, _chunk: bytes) -> None:
                nonlocal sink_calls
                sink_calls += 1
                time.sleep(0.08)

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            transport = HttpTransport(
                ClientOptions("https://xenoteer.test", "t" * 48),
                http_client=http_client,
            )
            for sink in (blocking_sink, BlockingSink()):
                with self.subTest(sink=type(sink).__name__):
                    with self.assertRaises(XenoteerError) as caught:
                        await transport.download_artifact(
                            f"/v1/artifacts/{ARTIFACT_ID}",
                            reference,
                            sink,
                        )
                    self.assertEqual(caught.exception.code, "invalid_request")
        self.assertEqual(sink_calls, 0)
        self.assertEqual(adapter_calls, 0)

    async def test_exported_artifacts_rejects_sync_sink_before_adapter_io(
        self,
    ) -> None:
        body = b"artifact sink policy"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        adapter_calls = 0
        sink_calls = 0

        class ArtifactTransport(Transport):
            async def download_artifact(
                self,
                _path: str,
                _artifact: object,
                sink: Callable[[bytes], Any],
            ) -> None:
                nonlocal adapter_calls
                adapter_calls += 1
                outcome = sink(body)
                if inspect.isawaitable(outcome):
                    await outcome

        def blocking_sink(_chunk: bytes) -> None:
            nonlocal sink_calls
            sink_calls += 1
            time.sleep(0.08)

        artifacts = Artifacts(
            ArtifactTransport(lambda _method, _path: status_wire()),
            DESKTOP_ID,
            GENERATION,
        )
        with self.assertRaises(XenoteerError) as caught:
            await artifacts.download_to(reference, blocking_sink)
        self.assertEqual(caught.exception.code, "invalid_request")
        self.assertEqual(sink_calls, 0)
        self.assertEqual(adapter_calls, 0)

    async def test_async_sink_stall_uses_request_deadline_and_cleans_up(
        self,
    ) -> None:
        body = b"artifact sink policy"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        sink_started = asyncio.Event()
        sink_finished = asyncio.Event()

        async def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": reference.content_type,
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=body,
            )

        async def stalled_sink(_chunk: bytes) -> None:
            sink_started.set()
            try:
                await asyncio.Event().wait()
            finally:
                sink_finished.set()

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    request_timeout=0.01,
                ),
                http_client=http_client,
            )
            with self.assertRaises(XenoteerError) as caught:
                await asyncio.wait_for(
                    transport.download_artifact(
                        f"/v1/artifacts/{ARTIFACT_ID}",
                        reference,
                        stalled_sink,
                    ),
                    0.1,
                )
        self.assertEqual(caught.exception.code, "request_timeout")
        self.assertTrue(sink_started.is_set())
        self.assertTrue(sink_finished.is_set())

    async def test_caller_cancellation_unwinds_async_callable_sink(self) -> None:
        body = b"artifact sink cancellation"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        sink_started = asyncio.Event()
        sink_finished = asyncio.Event()

        class StalledSink:
            async def __call__(self, _chunk: bytes) -> None:
                sink_started.set()
                try:
                    await asyncio.Event().wait()
                finally:
                    sink_finished.set()

        async def handler(_request: httpx.Request) -> httpx.Response:
            return httpx.Response(
                200,
                headers={
                    "content-type": reference.content_type,
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=body,
            )

        async with httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        ) as http_client:
            transport = HttpTransport(
                ClientOptions(
                    "https://xenoteer.test",
                    "t" * 48,
                    request_timeout=60,
                ),
                http_client=http_client,
            )
            operation = asyncio.create_task(
                transport.download_artifact(
                    f"/v1/artifacts/{ARTIFACT_ID}",
                    reference,
                    StalledSink(),
                )
            )
            await asyncio.wait_for(sink_started.wait(), 0.1)
            operation.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(operation, 0.1)
            self.assertTrue(sink_finished.is_set())
            self.assertFalse(http_client.is_closed)

    async def test_client_close_cancels_download_and_unwinds_async_sink(
        self,
    ) -> None:
        body = b"artifact sink client close"
        reference = ArtifactRef.from_wire(artifact_wire(body))
        sink_started = asyncio.Event()
        sink_finished = asyncio.Event()

        async def handler(request: httpx.Request) -> httpx.Response:
            if request.url.path == "/v1/status":
                return httpx.Response(
                    200,
                    headers={"content-type": "application/json"},
                    json=status_wire(),
                )
            return httpx.Response(
                200,
                headers={
                    "content-type": reference.content_type,
                    "content-length": str(len(body)),
                    "x-content-sha256": reference.sha256,
                },
                content=body,
            )

        async def stalled_sink(_chunk: bytes) -> None:
            sink_started.set()
            try:
                await asyncio.Event().wait()
            finally:
                sink_finished.set()

        http_client = httpx.AsyncClient(
            base_url="https://xenoteer.test",
            transport=httpx.MockTransport(handler),
        )
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                request_timeout=60,
            ),
            http_client=http_client,
        )
        operation = asyncio.create_task(
            client.desktop().artifacts.download_to(reference, stalled_sink)
        )
        await asyncio.wait_for(sink_started.wait(), 0.1)
        await asyncio.wait_for(client.close(), 0.1)
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(operation, 0.1)
        self.assertTrue(sink_finished.is_set())
        self.assertFalse(http_client.is_closed)
        await http_client.aclose()


class WebSocketPolicyTests(unittest.IsolatedAsyncioTestCase):
    async def test_factory_reusing_closed_physical_socket_never_closes_it_twice(
        self,
    ) -> None:
        first = Socket(
            [
                welcome_wire(),
                ConnectionError("drop-first"),
                welcome_wire(),
            ]
        )
        second = Socket([welcome_wire(), ConnectionError("drop-second")])
        candidates = [first, second, first]
        factory_calls = 0

        def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            socket = candidates[factory_calls]
            factory_calls += 1
            return socket

        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=factory,
            reconnect_policy=ReconnectPolicy(
                max_attempts=2,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.03,
        )
        async with asyncio.timeout(0.1):
            while factory_calls != 3:
                await asyncio.sleep(0)
        await asyncio.wait_for(session.close(), 0.1)
        self.assertEqual(first.close_calls, 1)
        self.assertEqual(second.close_calls, 1)

    async def test_socket_identity_markers_expire_after_socket_collection(
        self,
    ) -> None:
        first = Socket([welcome_wire(), ConnectionError("drop-first")])
        second = Socket([welcome_wire()])
        candidates = [first, second]

        def factory(_url: str, **_kwargs: Any) -> Socket:
            return candidates.pop(0)

        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=factory,
            reconnect_policy=ReconnectPolicy(
                max_attempts=1,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.03,
        )
        async with asyncio.timeout(0.1):
            while session._socket is not second:
                await asyncio.sleep(0)
        first_reference = weakref.ref(first)
        del first
        gc.collect()
        await asyncio.sleep(0)
        self.assertIsNone(first_reference())
        self.assertEqual(session._socket_owners.live_count, 1)
        await asyncio.wait_for(session.close(), 0.1)

    async def test_non_weak_referenceable_factory_socket_is_rejected_and_closed(
        self,
    ) -> None:
        class NonWeakSocket:
            __slots__ = ("close_calls", "received", "send_calls")

            def __init__(self) -> None:
                self.close_calls = 0
                self.received = [welcome_wire()]
                self.send_calls = 0

            async def send(self, _message: str) -> None:
                self.send_calls += 1

            async def recv(self) -> object:
                return self.received.pop(0)

            async def close(self, **_kwargs: Any) -> None:
                self.close_calls += 1

        socket = NonWeakSocket()
        session: EventSession | None = None
        try:
            with self.assertRaisesRegex(XenoteerError, "weak reference"):
                session = await EventSession.connect(
                    "wss://xenoteer.test/v1/ws",
                    "Bearer " + ("t" * 48),
                    hello_wire(),
                    websocket_factory=lambda *_args, **_kwargs: socket,
                    connect_timeout=0.03,
                )
        finally:
            if session is not None:
                await asyncio.wait_for(session.close(), 0.1)
        self.assertEqual(socket.send_calls, 0)
        self.assertEqual(socket.close_calls, 1)

    async def test_close_once_uses_socket_identity_across_four_generations(
        self,
    ) -> None:
        sockets = [
            Socket([welcome_wire(), ConnectionError(f"drop-{index}")])
            for index in range(3)
        ]
        sockets.append(Socket([welcome_wire()]))
        remaining = list(sockets)
        factory_calls = 0

        def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            factory_calls += 1
            return remaining.pop(0)

        with mock.patch("xenoteer.events.id", return_value=7, create=True):
            session = await EventSession.connect(
                "wss://xenoteer.test/v1/ws",
                "Bearer " + ("t" * 48),
                hello_wire(),
                websocket_factory=factory,
                reconnect_policy=ReconnectPolicy(
                    max_attempts=3,
                    initial_delay=0,
                    max_delay=0,
                    jitter_min=0,
                    jitter_max=0,
                ),
                connect_timeout=0.03,
            )
            async with asyncio.timeout(0.1):
                while factory_calls != 4:
                    await asyncio.sleep(0)
            async with asyncio.timeout(0.1):
                while [socket.close_calls for socket in sockets[:3]] != [1, 1, 1]:
                    await asyncio.sleep(0)
            self.assertEqual(
                [socket.close_calls for socket in sockets[:3]],
                [1, 1, 1],
            )
            await asyncio.wait_for(session.close(), 0.1)
            self.assertEqual(
                [socket.close_calls for socket in sockets],
                [1, 1, 1, 1],
            )

    async def test_close_ownership_does_not_retain_generation_history(
        self,
    ) -> None:
        sockets = [
            Socket([welcome_wire(), ConnectionError(f"drop-{index}")])
            for index in range(8)
        ]
        sockets.append(Socket([welcome_wire()]))
        remaining = list(sockets)
        factory_calls = 0

        def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            factory_calls += 1
            return remaining.pop(0)

        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=factory,
            reconnect_policy=ReconnectPolicy(
                max_attempts=8,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.03,
        )
        async with asyncio.timeout(0.2):
            while factory_calls != 9 or session._socket is not sockets[-1]:
                await asyncio.sleep(0)
        await asyncio.sleep(0)
        self.assertFalse(hasattr(session, "_closed_socket_ids"))
        self.assertIs(session._socket_owner.socket, sockets[-1])
        await asyncio.wait_for(session.close(), 0.1)
        self.assertEqual(
            [socket.close_calls for socket in sockets],
            [1] * len(sockets),
        )

    def test_only_exact_transient_peer_closes_are_reconnectable(self) -> None:
        class PeerClosed(ConnectionError):
            def __init__(self, code: int | None) -> None:
                super().__init__("peer closed")
                self.rcvd = SimpleNamespace(code=code, reason="peer closed")

        for code in (None, 1001, 1012, 1013):
            with self.subTest(code=code, disposition="transient"):
                self.assertTrue(events_module._close_info(PeerClosed(code)).reconnectable)
        for code in (
            1000,
            1002,
            1003,
            1006,
            1007,
            1008,
            1009,
            1010,
            1011,
            1014,
            3000,
            4000,
            4401,
            4403,
        ):
            with self.subTest(code=code, disposition="terminal"):
                self.assertFalse(events_module._close_info(PeerClosed(code)).reconnectable)

    async def test_terminal_close_never_creates_replacement_socket(self) -> None:
        class PeerClosed(ConnectionError):
            def __init__(self, code: int) -> None:
                super().__init__("peer closed")
                self.rcvd = SimpleNamespace(code=code, reason="peer closed")

        sockets = [Socket([welcome_wire(), PeerClosed(1006)]), Socket([welcome_wire()])]
        factory_calls = 0

        def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            socket = sockets[factory_calls]
            factory_calls += 1
            return socket

        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=factory,
            reconnect_policy=ReconnectPolicy(
                max_attempts=1,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.03,
        )
        try:
            with self.assertRaises(XenoteerError):
                await asyncio.wait_for(anext(session), 0.1)
            self.assertEqual(factory_calls, 1)
            self.assertEqual(sockets[1].close_calls, 0)
        finally:
            await asyncio.wait_for(session.close(), 0.1)

    async def test_transient_close_reconnects_once_but_prewelcome_does_not(
        self,
    ) -> None:
        class PeerClosed(ConnectionError):
            def __init__(self, code: int) -> None:
                super().__init__("peer closed")
                self.rcvd = SimpleNamespace(code=code, reason="peer closed")

        sockets = [Socket([welcome_wire(), PeerClosed(1012)]), Socket([welcome_wire()])]
        factory_calls = 0

        def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            socket = sockets[factory_calls]
            factory_calls += 1
            return socket

        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=factory,
            reconnect_policy=ReconnectPolicy(
                max_attempts=1,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.03,
        )
        try:
            async with asyncio.timeout(0.1):
                while factory_calls != 2 or sockets[0].close_calls != 1:
                    await asyncio.sleep(0)
            self.assertEqual(sockets[0].close_calls, 1)
        finally:
            await asyncio.wait_for(session.close(), 0.1)

        prewelcome = Socket([PeerClosed(1012)])
        prewelcome_factory_calls = 0

        def prewelcome_factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal prewelcome_factory_calls
            prewelcome_factory_calls += 1
            return prewelcome

        with self.assertRaises(XenoteerError):
            await EventSession.connect(
                "wss://xenoteer.test/v1/ws",
                "Bearer " + ("t" * 48),
                hello_wire(),
                websocket_factory=prewelcome_factory,
                reconnect_policy=ReconnectPolicy(
                    max_attempts=1,
                    initial_delay=0,
                    max_delay=0,
                    jitter_min=0,
                    jitter_max=0,
                ),
                connect_timeout=0.03,
            )
        self.assertEqual(prewelcome_factory_calls, 1)
        self.assertEqual(prewelcome.close_calls, 1)

    async def test_sync_authorization_provider_is_rejected_before_factory_io(
        self,
    ) -> None:
        provider_calls = 0
        factory_calls = 0

        def provider() -> str:
            nonlocal provider_calls
            provider_calls += 1
            return "Bearer " + ("t" * 48)

        async def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            factory_calls += 1
            return Socket([welcome_wire()])

        with self.assertRaises(XenoteerError) as caught:
            await EventSession.connect(
                "wss://xenoteer.test/v1/ws",
                provider,
                hello_wire(),
                websocket_factory=factory,
            )
        self.assertEqual(caught.exception.code, "invalid_request")
        self.assertEqual(provider_calls, 0)
        self.assertEqual(factory_calls, 0)

    async def test_failed_handshake_bounds_blocked_socket_cleanup(self) -> None:
        close_cancelled = asyncio.Event()

        class BlockedClose(Socket):
            async def close(self, **kwargs: Any) -> None:
                del kwargs
                self.close_calls += 1
                try:
                    await asyncio.Event().wait()
                finally:
                    close_cancelled.set()

        socket = BlockedClose(["{}"])
        with self.assertRaises(XenoteerError):
            await asyncio.wait_for(
                EventSession.connect(
                    "wss://xenoteer.test/v1/ws",
                    "Bearer " + ("t" * 48),
                    hello_wire(),
                    websocket_factory=lambda *_args, **_kwargs: socket,
                    connect_timeout=0.01,
                ),
                0.1,
            )
        self.assertEqual(socket.close_calls, 1)
        self.assertTrue(close_cancelled.is_set())

    async def test_established_socket_send_and_close_are_bounded(self) -> None:
        send_cancelled = asyncio.Event()
        close_cancelled = asyncio.Event()
        send_release = asyncio.Event()
        close_release = asyncio.Event()

        class BlockedEstablished(Socket):
            async def send(self, message: str) -> None:
                if not self.sent:
                    self.sent.append(json.loads(message))
                    return
                try:
                    await send_release.wait()
                finally:
                    send_cancelled.set()

            async def close(self, **kwargs: Any) -> None:
                del kwargs
                self.close_calls += 1
                try:
                    await close_release.wait()
                finally:
                    close_cancelled.set()

        socket = BlockedEstablished([welcome_wire()])
        session = await asyncio.wait_for(
            EventSession.connect(
                "wss://xenoteer.test/v1/ws",
                "Bearer " + ("t" * 48),
                hello_wire(),
                websocket_factory=lambda *_args, **_kwargs: socket,
                connect_timeout=0.01,
            ),
            0.1,
        )
        try:
            with self.assertRaises(XenoteerError) as send_error:
                await asyncio.wait_for(
                    session.send({"type": "client.ping"}), 0.1
                )
            self.assertEqual(send_error.exception.code, "request_timeout")
            self.assertTrue(send_cancelled.is_set())
            await asyncio.wait_for(session.close(), 0.1)
            self.assertEqual(socket.close_calls, 1)
            self.assertTrue(close_cancelled.is_set())
        finally:
            send_release.set()
            close_release.set()
            session._closed = True
            for task in (session._reader, session._heartbeat, session._writer):
                if not task.done():
                    task.cancel()
            await asyncio.sleep(0)

    async def test_subscription_send_uses_its_one_deadline(self) -> None:
        send_cancelled = asyncio.Event()

        class BlockedSubscription(Socket):
            async def send(self, message: str) -> None:
                if not self.sent:
                    self.sent.append(json.loads(message))
                    return
                try:
                    await asyncio.Event().wait()
                finally:
                    send_cancelled.set()

        socket = BlockedSubscription([welcome_wire()])
        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=lambda *_args, **_kwargs: socket,
            connect_timeout=1,
        )
        try:
            with self.assertRaises(XenoteerError) as caught:
                await asyncio.wait_for(
                    session.subscribe(
                        DESKTOP_ID,
                        GENERATION,
                        ["window.created"],
                        timeout=0.01,
                    ),
                    0.1,
                )
            self.assertEqual(caught.exception.code, "request_timeout")
            async with asyncio.timeout(0.05):
                while not send_cancelled.is_set():
                    await asyncio.sleep(0)
        finally:
            await asyncio.wait_for(session.close(), 0.1)

    async def test_reconnect_bounds_resubscribe_and_old_socket_retirement(
        self,
    ) -> None:
        resubscribe_cancelled = asyncio.Event()

        class AcknowledgingSocket(Socket):
            async def send(self, message: str) -> None:
                decoded = json.loads(message)
                self.sent.append(decoded)
                if decoded["type"] == "events.subscribe":
                    self.received.put_nowait(
                        json.dumps(
                            {
                                "type": "events.subscribed",
                                "request_id": decoded["request_id"],
                                "topics": decoded["topics"],
                            }
                        )
                    )

        class BlockedResubscribe(Socket):
            async def send(self, message: str) -> None:
                decoded = json.loads(message)
                if not self.sent:
                    self.sent.append(decoded)
                    return
                try:
                    await asyncio.Event().wait()
                finally:
                    resubscribe_cancelled.set()

        first = AcknowledgingSocket([welcome_wire()])
        second = BlockedResubscribe([welcome_wire()])
        sockets = [first, second]
        session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=lambda *_args, **_kwargs: sockets.pop(0),
            reconnect_policy=ReconnectPolicy(
                max_attempts=1,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.01,
        )
        await session.subscribe(
            DESKTOP_ID,
            GENERATION,
            ["window.created"],
            timeout=0.05,
        )
        first.received.put_nowait(ConnectionError("drop"))
        with self.assertRaises(XenoteerError):
            await asyncio.wait_for(anext(session), 0.1)
        self.assertTrue(resubscribe_cancelled.is_set())
        self.assertEqual(first.close_calls, 1)
        self.assertEqual(second.close_calls, 1)

        retirement_cancelled = asyncio.Event()

        class BlockedRetirement(Socket):
            async def close(self, **kwargs: Any) -> None:
                del kwargs
                self.close_calls += 1
                try:
                    await asyncio.Event().wait()
                finally:
                    retirement_cancelled.set()

        class PeerClosed(ConnectionError):
            def __init__(self, code: int) -> None:
                super().__init__("peer closed")
                self.rcvd = SimpleNamespace(code=code, reason="peer closed")

        old = BlockedRetirement([welcome_wire()])
        replacement = Socket([welcome_wire(), PeerClosed(1000)])
        reconnect_sockets = [old, replacement]
        retirement_session = await EventSession.connect(
            "wss://xenoteer.test/v1/ws",
            "Bearer " + ("t" * 48),
            hello_wire(),
            websocket_factory=lambda *_args, **_kwargs: reconnect_sockets.pop(0),
            reconnect_policy=ReconnectPolicy(
                max_attempts=1,
                initial_delay=0,
                max_delay=0,
                jitter_min=0,
                jitter_max=0,
            ),
            connect_timeout=0.01,
        )
        old.received.put_nowait(ConnectionError("drop"))
        with self.assertRaises(XenoteerError):
            await asyncio.wait_for(anext(retirement_session), 0.1)
        self.assertTrue(retirement_cancelled.is_set())
        self.assertEqual(old.close_calls, 1)
        self.assertEqual(replacement.close_calls, 1)

    async def test_oversized_hello_is_rejected_before_factory_io(self) -> None:
        factory_calls = 0
        transport = Transport(lambda _method, _path: status_wire())

        async def factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal factory_calls
            factory_calls += 1
            return Socket([welcome_wire()])

        client = await XenoteerClient.connect(
            ClientOptions("https://xenoteer.test", "t" * 48),
            transport=transport,
            websocket_factory=factory,
        )
        with self.assertRaisesRegex(XenoteerError, "hello"):
            await client.open_events(
                resume={
                    "desktop_id": "x" * 1_048_577,
                    "desktop_generation": GENERATION,
                    "event_sequence": "1",
                }
            )
        self.assertEqual(factory_calls, 0)
        await client.close()

    async def test_initial_websocket_provider_and_welcome_share_one_deadline(
        self,
    ) -> None:
        never = asyncio.Event()
        logs: list[SafeLogEvent] = []

        class BlockingAuthorizationTransport(Transport):
            async def authorization_header(self) -> str:
                await never.wait()
                return "Bearer " + ("t" * 48)

        provider_transport = BlockingAuthorizationTransport(
            lambda _method, _path: status_wire()
        )
        provider_factory_calls = 0

        async def provider_factory(_url: str, **_kwargs: Any) -> Socket:
            nonlocal provider_factory_calls
            provider_factory_calls += 1
            return Socket([welcome_wire()])

        provider_client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                connect_timeout=0.01,
                safe_log_hook=logs.append,
            ),
            transport=provider_transport,
            websocket_factory=provider_factory,
        )
        with self.assertRaises(XenoteerError) as provider_timeout:
            await provider_client.open_events()
        self.assertEqual(provider_timeout.exception.code, "request_timeout")
        self.assertEqual(provider_factory_calls, 0)
        await provider_client.close()

        socket = Socket([])
        welcome_transport = Transport(lambda _method, _path: status_wire())
        welcome_client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                connect_timeout=0.01,
                safe_log_hook=logs.append,
            ),
            transport=welcome_transport,
            websocket_factory=lambda *_args, **_kwargs: socket,
        )
        with self.assertRaises(XenoteerError) as welcome_timeout:
            await welcome_client.open_events()
        self.assertEqual(welcome_timeout.exception.code, "request_timeout")
        self.assertEqual(socket.close_calls, 1)
        await welcome_client.close()
        websocket_logs = [
            event for event in logs if event.operation == "websocket.handshake"
        ]
        self.assertEqual(
            [(event.outcome, event.error_code) for event in websocket_logs],
            [
                ("started", None),
                ("failed", "request_timeout"),
                ("started", None),
                ("failed", "request_timeout"),
            ],
        )

    async def test_cancelled_initial_websocket_closes_candidate_once(self) -> None:
        socket = Socket([])
        logs: list[SafeLogEvent] = []
        transport = Transport(lambda _method, _path: status_wire())
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                connect_timeout=1,
                safe_log_hook=logs.append,
            ),
            transport=transport,
            websocket_factory=lambda *_args, **_kwargs: socket,
        )
        task = asyncio.create_task(client.open_events())
        for _ in range(100):
            if socket.sent:
                break
            await asyncio.sleep(0)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertEqual(socket.close_calls, 1)
        websocket_logs = [
            event for event in logs if event.operation == "websocket.handshake"
        ]
        self.assertEqual(
            [(event.outcome, event.error_code) for event in websocket_logs],
            [("started", None), ("failed", "cancelled")],
        )
        await client.close()

    async def test_reconnect_welcome_is_bounded_by_the_same_connect_deadline(
        self,
    ) -> None:
        first = Socket([welcome_wire()])
        second = Socket([])
        sockets = [first, second]
        logs: list[SafeLogEvent] = []

        async def factory(_url: str, **_kwargs: Any) -> Socket:
            return sockets.pop(0)

        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                connect_timeout=0.01,
                heartbeat_interval=300,
                reconnect_policy=ReconnectPolicy(
                    max_attempts=1,
                    initial_delay=0,
                    max_delay=0,
                    jitter_min=0,
                    jitter_max=0,
                ),
                safe_log_hook=logs.append,
            ),
            transport=Transport(lambda _method, _path: status_wire()),
            websocket_factory=factory,
        )
        session = await client.open_events()
        first.received.put_nowait(ConnectionError("drop"))
        with self.assertRaises(XenoteerError):
            await asyncio.wait_for(anext(session), 1)
        self.assertEqual(first.close_calls, 1)
        self.assertEqual(second.close_calls, 1)
        reconnect_logs = [
            event
            for event in logs
            if event.operation == "websocket.handshake" and event.attempt == 1
        ]
        self.assertEqual(
            [(event.outcome, event.error_code) for event in reconnect_logs],
            [("started", None), ("failed", "request_timeout")],
        )
        await client.close()

    async def test_factory_token_metadata_and_policy_are_retained_across_reconnect(
        self,
    ) -> None:
        first = Socket([welcome_wire()])
        second = Socket([welcome_wire()])
        sockets = [first, second]
        factory_calls: list[tuple[str, dict[str, Any]]] = []
        logs: list[SafeLogEvent] = []
        transport = Transport(
            lambda _method, _path: status_wire(),
            tokens=["a" * 48, "b" * 48],
        )

        async def factory(url: str, **kwargs: Any) -> Socket:
            factory_calls.append((url, kwargs))
            return sockets.pop(0)

        policy = ReconnectPolicy(
            max_attempts=1,
            initial_delay=0.25,
            max_delay=0.25,
            jitter_min=0.75,
            jitter_max=0.75,
        )
        options = ClientOptions(
            "https://xenoteer.test",
            "unused" * 8,
            client_name="connection-test",
            client_version="9.8.7",
            heartbeat_interval=300,
            reconnect_policy=policy,
            safe_log_hook=logs.append,
        )
        client = await XenoteerClient.connect(
            options,
            transport=transport,
            websocket_factory=factory,
        )
        real_sleep = asyncio.sleep
        with (
            mock.patch("xenoteer.events.asyncio.sleep", new=mock.AsyncMock()) as sleep,
            mock.patch("xenoteer.events.random.uniform", return_value=0.75) as uniform,
        ):
            session = await client.open_events()
            first.received.put_nowait(ConnectionError("dropped transport canary"))
            for _ in range(100):
                if (
                    len(factory_calls) == 2
                    and session._socket is second
                    and first.close_calls == 1
                ):
                    break
                await real_sleep(0)
            self.assertEqual(len(factory_calls), 2)
            self.assertIn(mock.call(0.1875), sleep.await_args_list)
            uniform.assert_called_once_with(0.75, 0.75)
            await session.close()
        await client.close()

        self.assertEqual(transport.authorization_calls, 2)
        self.assertEqual(
            [call[0] for call in factory_calls],
            ["wss://xenoteer.test/v1/ws", "wss://xenoteer.test/v1/ws"],
        )
        for _url, kwargs in factory_calls:
            self.assertEqual(kwargs["max_size"], 1_048_576)
            self.assertEqual(kwargs["max_queue"], 16)
            self.assertIsNone(kwargs["compression"])
            self.assertEqual(kwargs["open_timeout"], 10)
        self.assertEqual(first.sent[0]["client"], second.sent[0]["client"])
        self.assertEqual(first.sent[0]["protocol"], second.sent[0]["protocol"])
        self.assertEqual(first.close_calls, 1)
        self.assertEqual(second.close_calls, 1)
        self.assertEqual(
            [
                (event.operation, event.outcome, event.attempt)
                for event in logs
                if event.operation == "websocket.handshake"
            ],
            [
                ("websocket.handshake", "started", 0),
                ("websocket.handshake", "succeeded", 0),
                ("websocket.handshake", "started", 1),
                ("websocket.handshake", "succeeded", 1),
            ],
        )
        self.assertIs(options.reconnect_policy, policy)

    async def test_failed_handshake_closes_each_candidate_exactly_once(self) -> None:
        socket = Socket(["not JSON"])
        logs: list[SafeLogEvent] = []
        transport = Transport(lambda _method, _path: status_wire())
        client = await XenoteerClient.connect(
            ClientOptions(
                "https://xenoteer.test",
                "t" * 48,
                safe_log_hook=logs.append,
            ),
            transport=transport,
            websocket_factory=lambda *_args, **_kwargs: socket,
        )
        with self.assertRaises(XenoteerError):
            await client.open_events()
        self.assertEqual(socket.close_calls, 1)
        self.assertEqual(
            [
                event.outcome
                for event in logs
                if event.operation == "websocket.handshake"
            ],
            ["started", "failed"],
        )
        await client.close()
