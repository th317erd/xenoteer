# SPDX-License-Identifier: Apache-2.0
"""Bounded async event streaming with explicit continuity barriers."""

from __future__ import annotations

import asyncio
import copy
import ipaddress
import inspect
import json
import random
import re
import time
import uuid
from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass, field
from types import TracebackType
from typing import Any, Literal, Protocol, TypeAlias, cast
from urllib.parse import urlsplit

from .errors import XenoteerError
from .protocol_generated import EventMessageWire, JsonObject, JsonValue
from .wire import as_uint64_string, decode_uint64, encode_uint64, validate_uint64_fields

if False:  # pragma: no cover - typing-only cycle guard
    from .state import GenerationRegistry


MAX_WEBSOCKET_MESSAGE_BYTES = 1_048_576
_EVENT_TOPIC = re.compile(
    r"[a-z0-9](?:[a-z0-9_-]{0,126}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9_-]{0,126}[a-z0-9])?)*\Z"
)
_KNOWN_TOPICS = frozenset(
    {
        "command.lifecycle",
        "action.lifecycle",
        "process.exited",
        "window.created",
        "window.changed",
        "window.removed",
        "window.focus",
        "clipboard.changed",
        "accessibility.element_created",
        "accessibility.element_changed",
        "accessibility.element_removed",
        "accessibility.resync_required",
    }
)


@dataclass(frozen=True, slots=True)
class KnownEvent:
    """Known v1 topic with complete additive wire data retained."""

    topic: str
    sequence: int
    _payload: JsonValue = field(repr=False)
    _raw: EventMessageWire = field(repr=False)
    kind: Literal["known"] = field(default="known", init=False)

    @property
    def payload(self) -> JsonValue:
        return copy.deepcopy(self._payload)

    @property
    def raw(self) -> EventMessageWire:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class UnknownEvent:
    """Future topic preserved exactly instead of closing the connection."""

    topic: str
    sequence: int
    _payload: JsonValue = field(repr=False)
    _raw: EventMessageWire = field(repr=False)
    kind: Literal["unknown"] = field(default="unknown", init=False)

    @property
    def payload(self) -> JsonValue:
        return copy.deepcopy(self._payload)

    @property
    def raw(self) -> EventMessageWire:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class SubscriptionAck:
    request_id: str
    topics: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class ReplayComplete:
    desktop_id: str
    desktop_generation: str
    through_sequence: int
    _raw: JsonObject = field(repr=False)
    kind: Literal["replay_complete"] = field(default="replay_complete", init=False)

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class ResyncRequired:
    desktop_id: str
    desktop_generation: str
    reason: str
    dropped_through: int
    latest_sequence: int
    _raw: JsonObject = field(repr=False)
    kind: Literal["resync_required"] = field(default="resync_required", init=False)

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class ServerDraining:
    desktop_id: str
    desktop_generation: str | None
    reason_code: str | None
    _raw: JsonObject = field(repr=False)
    kind: Literal["server_draining"] = field(default="server_draining", init=False)

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class ServerError:
    code: str
    request_id: str | None
    desktop_generation: str | None
    _detail: str = field(repr=False)
    _raw: JsonObject = field(repr=False)
    kind: Literal["server_error"] = field(default="server_error", init=False)

    @property
    def detail(self) -> str:
        return self._detail

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class UnknownServerMessage:
    message_type: str
    _raw: JsonObject = field(repr=False)
    kind: Literal["unknown_server_message"] = field(
        default="unknown_server_message", init=False
    )

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


@dataclass(frozen=True, slots=True)
class CloseInfo:
    code: int | None
    reason: str | None
    reconnectable: bool


@dataclass(frozen=True, slots=True)
class ServerWelcome:
    protocol_major: int
    protocol_minor: int
    desktop_id: str
    desktop_generation: str | None
    max_message_bytes: int
    heartbeat_seconds: float
    resume_status: str
    _raw: JsonObject = field(repr=False)

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


XenoteerEvent: TypeAlias = KnownEvent | UnknownEvent
EventItem: TypeAlias = (
    KnownEvent
    | UnknownEvent
    | ReplayComplete
    | ResyncRequired
    | ServerDraining
    | ServerError
    | UnknownServerMessage
)


def decode_event_message(value: object) -> XenoteerEvent:
    """Decode one event while retaining exact decimal sequence text and raw data."""

    if (
        not isinstance(value, dict)
        or value.get("type") != "event"
        or not isinstance(value.get("request_id"), str)
    ):
        raise XenoteerError("invalid_response", "invalid Xenoteer event envelope")
    event = value.get("event")
    if (
        not isinstance(event, dict)
        or not isinstance(event.get("desktop_id"), str)
        or not isinstance(event.get("desktop_generation"), str)
        or not isinstance(event.get("topic"), str)
        or "payload" not in event
    ):
        raise XenoteerError("invalid_response", "invalid Xenoteer event")
    try:
        sequence = decode_uint64(event.get("sequence"), allow_zero=False)
        validate_uint64_fields(value)
    except (TypeError, ValueError):
        raise XenoteerError(
            "invalid_response", "event contains an invalid uint64 wire value"
        ) from None
    raw = copy.deepcopy(value)
    raw["event"]["sequence"] = as_uint64_string(
        event["sequence"], allow_zero=False
    )
    event_type = KnownEvent if event["topic"] in _KNOWN_TOPICS else UnknownEvent
    return event_type(
        topic=event["topic"],
        sequence=sequence,
        _payload=copy.deepcopy(event["payload"]),
        _raw=raw,  # type: ignore[arg-type]
    )


def decode_server_message(value: object) -> EventItem | SubscriptionAck:
    """Decode one complete server message without transport side effects."""

    if not isinstance(value, dict) or not isinstance(value.get("type"), str):
        raise XenoteerError("invalid_response", "WebSocket envelope is invalid")
    message_type = value["type"]
    if message_type == "event":
        return decode_event_message(value)
    if message_type == "events.replay_complete":
        return _decode_replay(value)
    if message_type == "events.resync_required":
        return _decode_resync(value)
    if message_type == "server.draining":
        return _decode_draining(value)
    if message_type == "error":
        return _decode_server_error(value)
    if message_type == "events.subscribed":
        request_id = value.get("request_id")
        topics = value.get("topics")
        if (
            not isinstance(request_id, str)
            or not isinstance(topics, list)
            or any(not isinstance(topic, str) for topic in topics)
        ):
            raise XenoteerError(
                "invalid_response", "subscription acknowledgement is invalid"
            )
        return SubscriptionAck(request_id, tuple(topics))
    return UnknownServerMessage(message_type, copy.deepcopy(value))


class WebSocketLike(Protocol):
    async def send(self, message: str) -> Any: ...

    async def recv(self) -> object: ...

    async def close(self, **kwargs: Any) -> Any: ...


WebSocketFactory: TypeAlias = Callable[
    ..., WebSocketLike | Awaitable[WebSocketLike]
]
AuthorizationSource: TypeAlias = str | Callable[[], str | Awaitable[str]]
_SENTINEL = object()


@dataclass(slots=True)
class _WriteRequest:
    encoded: str
    completed: asyncio.Future[None]


class EventSession:
    """One authenticated WebSocket with bounded queueing and explicit resync."""

    __slots__ = (
        "_authorization_source",
        "_active_subscription",
        "_closed",
        "_close_info",
        "_control_item",
        "_error",
        "_factory",
        "_heartbeat",
        "_heartbeat_interval",
        "_hello",
        "_last_desktop_generation",
        "_last_desktop_id",
        "_last_received",
        "_last_sequence",
        "_max_reconnect_attempts",
        "_max_message_bytes",
        "_pending",
        "_permanent",
        "_queue",
        "_queue_capacity",
        "_read_stale_timeout",
        "_reader",
        "_registry",
        "_socket",
        "_subscription",
        "_url",
        "_welcome",
        "_writer",
        "_writer_queue",
    )
    _socket: WebSocketLike

    def __init__(
        self,
        socket: WebSocketLike,
        welcome: ServerWelcome,
        capacity: int,
        *,
        url: str,
        authorization_source: AuthorizationSource,
        hello: Mapping[str, Any],
        factory: WebSocketFactory,
        heartbeat_interval: float,
        read_stale_timeout: float,
        max_reconnect_attempts: int,
        registry: "GenerationRegistry | None",
    ) -> None:
        self._socket = socket
        self._welcome = welcome
        # The physical extra slot is reserved for the iterator sentinel. A
        # separate control slot preserves an authoritative terminal frame even
        # when every caller-visible queue position is occupied.
        self._queue_capacity = capacity
        self._queue: asyncio.Queue[EventItem | object] = asyncio.Queue(capacity + 1)
        self._control_item: EventItem | None = None
        self._closed = False
        self._error: XenoteerError | None = None
        self._close_info: CloseInfo | None = None
        self._url = url
        self._authorization_source = authorization_source
        self._hello = copy.deepcopy(dict(hello))
        self._factory = factory
        self._heartbeat_interval = welcome.heartbeat_seconds
        self._read_stale_timeout = max(
            read_stale_timeout,
            welcome.heartbeat_seconds * 3,
            welcome.heartbeat_seconds + 1,
        )
        self._max_reconnect_attempts = max_reconnect_attempts
        self._max_message_bytes = min(
            MAX_WEBSOCKET_MESSAGE_BYTES, welcome.max_message_bytes
        )
        self._permanent = False
        self._registry = registry
        if registry is not None and welcome.desktop_generation is not None:
            registry.observe(welcome.desktop_generation)
        self._last_received = time.monotonic()
        self._last_sequence: str | None = None
        self._last_desktop_id: str | None = None
        self._last_desktop_generation: str | None = None
        self._subscription: dict[str, Any] | None = None
        self._active_subscription: dict[str, Any] | None = None
        self._pending: dict[
            str, tuple[dict[str, Any], asyncio.Future[SubscriptionAck]]
        ] = {}
        self._writer_queue: asyncio.Queue[_WriteRequest] = asyncio.Queue(32)
        self._writer = asyncio.create_task(
            self._writer_loop(), name="xenoteer-event-writer"
        )
        self._reader = asyncio.create_task(self._read(), name="xenoteer-event-reader")
        self._heartbeat = asyncio.create_task(
            self._heartbeat_loop(), name="xenoteer-event-heartbeat"
        )

    @property
    def close_info(self) -> CloseInfo | None:
        return self._close_info

    @property
    def welcome(self) -> ServerWelcome:
        return self._welcome

    @property
    def resume_cursor(self) -> str | None:
        """Last configured or successfully admitted event/replay cursor."""

        return self._last_sequence

    @classmethod
    async def connect(
        cls,
        url: str,
        authorization: AuthorizationSource,
        hello: Mapping[str, Any],
        *,
        capacity: int = 256,
        websocket_factory: WebSocketFactory | None = None,
        heartbeat_interval: float = 15.0,
        read_stale_timeout: float = 45.0,
        max_reconnect_attempts: int = 5,
        registry: "GenerationRegistry | None" = None,
    ) -> "EventSession":
        _validate_websocket_url(url)
        if isinstance(capacity, bool) or not isinstance(capacity, int) or not 1 <= capacity <= 4096:
            raise XenoteerError("invalid_request", "event capacity must be in 1..4096")
        for label, value in (
            ("heartbeat interval", heartbeat_interval),
            ("read stale timeout", read_stale_timeout),
        ):
            if (
                isinstance(value, bool)
                or not isinstance(value, (int, float))
                or not 1 <= value <= 600
            ):
                raise XenoteerError(
                    "invalid_request", f"{label} must be in [1, 600] seconds"
                )
        if read_stale_timeout <= heartbeat_interval:
            raise XenoteerError(
                "invalid_request", "read stale timeout must exceed heartbeat interval"
            )
        if (
            isinstance(max_reconnect_attempts, bool)
            or not isinstance(max_reconnect_attempts, int)
            or not 0 <= max_reconnect_attempts <= 20
        ):
            raise XenoteerError(
                "invalid_request", "max reconnect attempts must be in 0..20"
            )
        factory = websocket_factory
        if factory is None:
            try:
                from websockets.asyncio.client import connect
            except ImportError:
                raise XenoteerError(
                    "missing_dependency", "websockets is required for event sessions"
                ) from None
            factory = cast(WebSocketFactory, connect)
        socket, welcome = await cls._open_socket(factory, url, authorization, hello)
        return cls(
            socket,
            welcome,
            capacity,
            url=url,
            authorization_source=authorization,
            hello=hello,
            factory=factory,
            heartbeat_interval=float(heartbeat_interval),
            read_stale_timeout=float(read_stale_timeout),
            max_reconnect_attempts=max_reconnect_attempts,
            registry=registry,
        )

    @staticmethod
    async def _open_socket(
        factory: WebSocketFactory,
        url: str,
        authorization_source: AuthorizationSource,
        hello: Mapping[str, Any],
    ) -> tuple[WebSocketLike, ServerWelcome]:
        authorization = await _resolve_authorization(authorization_source)
        try:
            candidate = factory(
                url,
                additional_headers={"authorization": authorization},
                max_size=MAX_WEBSOCKET_MESSAGE_BYTES,
                max_queue=16,
                compression=None,
                open_timeout=10,
            )
            socket = (
                await candidate
                if inspect.isawaitable(candidate)
                else candidate
            )
            await socket.send(json.dumps(hello, separators=(",", ":"), ensure_ascii=False))
            first = await asyncio.wait_for(socket.recv(), timeout=10)
            if (
                not isinstance(first, str)
                or len(first.encode("utf-8")) > MAX_WEBSOCKET_MESSAGE_BYTES
            ):
                raise XenoteerError(
                    "invalid_response", "server welcome frame is invalid"
                )
            try:
                decoded = json.loads(first)
            except (json.JSONDecodeError, RecursionError):
                raise XenoteerError(
                    "invalid_response", "server welcome is invalid JSON"
                ) from None
            welcome = _decode_welcome(decoded, hello)
            return socket, welcome
        except XenoteerError:
            raise
        except Exception as error:
            raise _websocket_error(error, connecting=True) from None

    async def subscribe(
        self,
        desktop_id: str,
        desktop_generation: str,
        topics: list[str],
        *,
        since_sequence: int | str | None = None,
        timeout: float = 10.0,
        request_id: str | None = None,
    ) -> SubscriptionAck:
        if (
            not isinstance(topics, list)
            or len(topics) > 32
            or len(set(topics)) != len(topics)
            or any(
                not isinstance(topic, str)
                or not 1 <= len(topic.encode("utf-8")) <= 128
                or _EVENT_TOPIC.fullmatch(topic) is None
                for topic in topics
            )
        ):
            raise XenoteerError("invalid_request", "event topics are invalid")
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not 0 < timeout <= 10
        ):
            raise XenoteerError(
                "invalid_request", "subscribe timeout must be in (0, 10] seconds"
            )
        if (
            desktop_id != self._welcome.desktop_id
            or desktop_generation != self._welcome.desktop_generation
        ):
            raise XenoteerError(
                "invalid_request",
                "subscription desktop scope differs from the active session",
            )
        selected_request_id = _new_id() if request_id is None else request_id
        _validate_request_id(selected_request_id)
        if selected_request_id in self._pending:
            raise XenoteerError(
                "invalid_request", "subscription request ID is already pending"
            )
        message: dict[str, Any] = {
            "type": "events.subscribe",
            "request_id": selected_request_id,
            "desktop_id": desktop_id,
            "desktop_generation": desktop_generation,
            "topics": list(topics),
            "since_sequence": None,
        }
        if since_sequence is not None:
            message["since_sequence"] = (
                encode_uint64(since_sequence, allow_zero=True)
                if isinstance(since_sequence, int) and not isinstance(since_sequence, bool)
                else as_uint64_string(since_sequence, allow_zero=True)
            )
        future = asyncio.get_running_loop().create_future()
        future.add_done_callback(_consume_future_exception)
        self._pending[selected_request_id] = (copy.deepcopy(message), future)
        try:
            await self.send(message)
            return await asyncio.wait_for(future, float(timeout))
        except TimeoutError:
            raise XenoteerError(
                "request_timeout", "event subscription acknowledgement timed out"
            ) from None
        finally:
            self._pending.pop(selected_request_id, None)

    async def send(self, message: Mapping[str, Any]) -> None:
        if self._closed:
            raise XenoteerError("transport", "event session is closed")
        try:
            encoded = json.dumps(
                message, separators=(",", ":"), ensure_ascii=False, allow_nan=False
            )
        except (TypeError, ValueError):
            raise XenoteerError("invalid_request", "WebSocket message is invalid") from None
        if len(encoded.encode("utf-8")) > self._max_message_bytes:
            raise XenoteerError("invalid_request", "WebSocket message is too large")
        completed: asyncio.Future[None] = asyncio.get_running_loop().create_future()
        try:
            self._writer_queue.put_nowait(_WriteRequest(encoded, completed))
        except asyncio.QueueFull:
            raise XenoteerError(
                "backpressure", "WebSocket writer queue is full"
            ) from None
        await completed

    async def _writer_loop(self) -> None:
        try:
            while True:
                request = await self._writer_queue.get()
                if request.completed.cancelled():
                    continue
                try:
                    await self._socket.send(request.encoded)
                except Exception as error:
                    failure = _websocket_error(error)
                    if not request.completed.done():
                        request.completed.set_exception(failure)
                else:
                    if not request.completed.done():
                        request.completed.set_result(None)
        except asyncio.CancelledError:
            pass
        finally:
            while not self._writer_queue.empty():
                request = self._writer_queue.get_nowait()
                if not request.completed.done():
                    request.completed.set_exception(
                        XenoteerError("transport", "event writer is closed")
                    )

    async def _read(self) -> None:
        reconnect_attempt = 0
        try:
            while not self._closed:
                try:
                    message = await self._socket.recv()
                except asyncio.CancelledError:
                    raise
                except Exception as error:
                    if self._closed:
                        break
                    failure = _websocket_error(error)
                    self._close_info = _close_info(error)
                    if self._permanent or not self._close_info.reconnectable:
                        raise failure
                    while True:
                        reconnect_attempt += 1
                        if reconnect_attempt > self._max_reconnect_attempts:
                            raise XenoteerError(
                                "transport",
                                "event WebSocket reconnect budget exhausted",
                                source=error,
                            ) from None
                        try:
                            await self._reconnect(reconnect_attempt)
                        except asyncio.CancelledError:
                            raise
                        except XenoteerError as reconnect_error:
                            if reconnect_error.code in {
                                "authentication",
                                "permission",
                                "unsupported_version",
                                "generation_changed",
                            }:
                                raise
                            continue
                        break
                    continue
                reconnect_attempt = 0
                self._last_received = time.monotonic()
                if not isinstance(message, str):
                    raise XenoteerError(
                        "invalid_response", "binary WebSocket messages are unsupported"
                    )
                if len(message.encode("utf-8")) > self._max_message_bytes:
                    raise XenoteerError(
                        "response_too_large", "WebSocket message exceeded its bound"
                    )
                try:
                    decoded = json.loads(message)
                except (json.JSONDecodeError, RecursionError):
                    raise XenoteerError(
                        "invalid_response", "WebSocket returned invalid JSON"
                    ) from None
                await self._dispatch(decoded)
        except asyncio.CancelledError:
            pass
        except XenoteerError as error:
            self._error = error
        except Exception:
            if not self._closed:
                self._error = XenoteerError("transport", "event WebSocket closed")
        finally:
            self._closed = True
            if not self._heartbeat.done():
                self._heartbeat.cancel()
            if not self._writer.done():
                self._writer.cancel()
            for _, pending in self._pending.values():
                if not pending.done():
                    pending.set_exception(
                        self._error
                        or XenoteerError("transport", "event session is closed")
                    )
            self._pending.clear()
            self._queue.put_nowait(_SENTINEL)

    async def _dispatch(self, decoded: object) -> None:
        if not isinstance(decoded, dict) or not isinstance(decoded.get("type"), str):
            raise XenoteerError("invalid_response", "WebSocket envelope is invalid")
        message_type = decoded["type"]
        if message_type == "server.pong":
            if (
                not isinstance(decoded.get("request_id"), str)
                or not isinstance(decoded.get("nonce"), str)
                or not decoded["nonce"]
            ):
                raise XenoteerError(
                    "invalid_response", "server pong is invalid"
                )
            return
        if message_type == "server.welcome":
            raise XenoteerError(
                "invalid_response", "duplicate server welcome is invalid"
            )
        if message_type == "events.subscribed":
            request_id = decoded.get("request_id")
            topics = decoded.get("topics")
            if (
                not isinstance(request_id, str)
                or not isinstance(topics, list)
                or any(not isinstance(topic, str) for topic in topics)
            ):
                raise XenoteerError(
                    "invalid_response", "subscription acknowledgement is invalid"
                )
            pending = self._pending.get(request_id)
            if pending is None:
                raise XenoteerError(
                    "invalid_response",
                    "subscription acknowledgement has no active request",
                )
            subscription, future = pending
            observed = tuple(topics)
            if observed != tuple(subscription["topics"]):
                if not future.done():
                    future.set_exception(
                        XenoteerError(
                            "invalid_response",
                            "subscription acknowledgement topics differed",
                        )
                    )
                raise XenoteerError(
                    "invalid_response",
                    "subscription acknowledgement topics differed",
                )
            self._subscription = copy.deepcopy(subscription)
            self._active_subscription = copy.deepcopy(subscription)
            self._last_desktop_id = subscription["desktop_id"]
            self._last_desktop_generation = subscription["desktop_generation"]
            if subscription["since_sequence"] is not None:
                self._last_sequence = subscription["since_sequence"]
            if not future.done():
                future.set_result(SubscriptionAck(request_id, observed))
            return
        if message_type == "event":
            event = decode_event_message(decoded)
            raw_event = event.raw["event"]
            self._require_active_subscription(
                decoded["request_id"],
                raw_event["desktop_id"],
                raw_event["desktop_generation"],
                topic=event.topic,
            )
            sequence = event.sequence
            if self._last_sequence is not None:
                previous = decode_uint64(self._last_sequence, allow_zero=True)
                if sequence == previous:
                    return
                if sequence < previous:
                    self._put_control(
                        ResyncRequired(
                            raw_event["desktop_id"],
                            raw_event["desktop_generation"],
                            "sequence_regression",
                            sequence,
                            previous,
                            copy.deepcopy(decoded),
                        )
                    )
                    self._subscription = None
                    self._active_subscription = None
                    raise XenoteerError(
                        "resync_required",
                        "event sequence regressed; fetch authoritative snapshots",
                    )
            self._put(event)
            self._last_sequence = raw_event["sequence"]
            self._last_desktop_id = raw_event["desktop_id"]
            self._last_desktop_generation = raw_event["desktop_generation"]
            return
        if message_type == "events.replay_complete":
            replay_item = _decode_replay(decoded)
            self._require_active_subscription(
                decoded.get("request_id"),
                replay_item.desktop_id,
                replay_item.desktop_generation,
            )
            if self._last_sequence is not None:
                previous = decode_uint64(self._last_sequence, allow_zero=True)
                if replay_item.through_sequence < previous:
                    raise XenoteerError(
                        "resync_required",
                        "replay boundary regressed; fetch authoritative snapshots",
                    )
            self._put(replay_item)
            self._last_sequence = as_uint64_string(
                decoded["through_sequence"], allow_zero=True
            )
            self._last_desktop_id = replay_item.desktop_id
            self._last_desktop_generation = replay_item.desktop_generation
            return
        if message_type == "events.resync_required":
            resync_item = _decode_resync(decoded)
            self._require_active_subscription(
                decoded.get("request_id"),
                resync_item.desktop_id,
                resync_item.desktop_generation,
            )
            self._put_control(resync_item)
            self._subscription = None
            self._active_subscription = None
            if resync_item.reason == "generation_changed":
                if self._registry is not None:
                    self._registry.invalidate("event_generation_changed")
                raise XenoteerError(
                    "generation_changed",
                    "desktop generation changed; discard generation-bound handles",
                )
            raise XenoteerError(
                "resync_required",
                f"event continuity ended: {resync_item.reason}; fetch authoritative snapshots",
            )
        if message_type == "server.draining":
            draining_item = _decode_draining(decoded)
            self._put_control(draining_item)
            self._permanent = True
            raise XenoteerError("server_draining", "server is draining")
        if message_type == "error":
            error_item = _decode_server_error(decoded)
            self._put(error_item)
            request_id = error_item.request_id
            pending_entry = self._pending.get(request_id or "")
            pending_future = None if pending_entry is None else pending_entry[1]
            failure = XenoteerError(
                "authentication"
                if error_item.code in {"authentication", "invalid_token"}
                else "permission"
                if error_item.code in {"permission_denied", "forbidden"}
                else error_item.code,
                "Xenoteer WebSocket request failed",
                request_id=request_id,
                problem_code=error_item.code,
            )
            if pending_future is not None and not pending_future.done():
                pending_future.set_exception(failure)
            if failure.code in {
                "authentication",
                "permission",
                "unsupported_version",
                "invalid_request",
            }:
                self._permanent = True
                raise failure
            return
        self._put(UnknownServerMessage(message_type, copy.deepcopy(decoded)))

    def _require_event_scope(self, desktop_id: str, generation: str) -> None:
        expected_id = self._welcome.desktop_id
        expected_generation = self._welcome.desktop_generation
        if desktop_id != expected_id:
            raise XenoteerError(
                "invalid_response", "event belongs to another desktop"
            )
        if expected_generation is not None and generation != expected_generation:
            if self._registry is not None:
                self._registry.observe(generation)
            raise XenoteerError(
                "generation_changed",
                "event belongs to another desktop generation",
            )

    def _require_active_subscription(
        self,
        request_id: object,
        desktop_id: str,
        generation: str,
        *,
        topic: str | None = None,
    ) -> None:
        subscription = self._active_subscription
        if subscription is None:
            raise XenoteerError(
                "invalid_response", "event stream has no active subscription"
            )
        if request_id != subscription["request_id"]:
            raise XenoteerError(
                "invalid_response", "event stream request ID is stale or unknown"
            )
        if (
            desktop_id != subscription["desktop_id"]
            or generation != subscription["desktop_generation"]
        ):
            self._require_event_scope(desktop_id, generation)
            raise XenoteerError(
                "invalid_response", "event stream scope differs from its subscription"
            )
        self._require_event_scope(desktop_id, generation)
        topics = subscription["topics"]
        if topic is not None and topics and topic not in topics:
            raise XenoteerError(
                "invalid_response", "event topic was not included in the subscription"
            )

    def _put(self, item: EventItem) -> None:
        if self._queue.qsize() >= self._queue_capacity:
            raise XenoteerError(
                "backpressure",
                "local event queue overflowed; fetch a fresh snapshot",
            ) from None
        self._queue.put_nowait(item)

    def _put_control(self, item: EventItem) -> None:
        if self._control_item is None:
            self._control_item = item

    async def _reconnect(self, attempt: int) -> None:
        """Reconnect event transport only; command mutations are never replayed."""

        delay = min(5.0, 0.1 * (2 ** (attempt - 1)))
        await asyncio.sleep(delay * random.uniform(0.5, 1.0))
        hello = copy.deepcopy(self._hello)
        if (
            self._last_sequence is not None
            and self._last_desktop_id is not None
            and self._last_desktop_generation is not None
        ):
            hello["request_id"] = _new_id()
            hello["resume"] = {
                "desktop_id": self._last_desktop_id,
                "desktop_generation": self._last_desktop_generation,
                "event_sequence": self._last_sequence,
            }
        socket, welcome = await self._open_socket(
            self._factory, self._url, self._authorization_source, hello
        )
        if (
            welcome.protocol_major != self._welcome.protocol_major
            or welcome.protocol_minor != self._welcome.protocol_minor
        ):
            await socket.close(code=1002, reason="protocol changed")
            raise XenoteerError(
                "unsupported_version", "protocol changed across reconnect"
            )
        if welcome.desktop_generation != self._welcome.desktop_generation:
            if self._registry is not None:
                self._registry.invalidate("reconnect_generation_changed")
            await socket.close(code=1002, reason="generation changed")
            raise XenoteerError(
                "generation_changed", "desktop generation changed across reconnect"
            )
        try:
            await self._socket.close(code=1012, reason="event reconnect")
        except Exception:
            pass
        self._socket = socket
        self._welcome = welcome
        self._max_message_bytes = min(
            MAX_WEBSOCKET_MESSAGE_BYTES, welcome.max_message_bytes
        )
        self._last_received = time.monotonic()
        if self._subscription is not None:
            subscription = copy.deepcopy(self._subscription)
            request_id = _new_id()
            subscription["request_id"] = request_id
            if self._last_sequence is not None:
                subscription["since_sequence"] = self._last_sequence
            await self.send(subscription)
            try:
                encoded_ack = await asyncio.wait_for(self._socket.recv(), timeout=10)
            except Exception as error:
                raise _websocket_error(error) from None
            if (
                not isinstance(encoded_ack, str)
                or len(encoded_ack.encode("utf-8")) > self._max_message_bytes
            ):
                raise XenoteerError(
                    "invalid_response", "resubscription acknowledgement is invalid"
                )
            try:
                ack_wire = json.loads(encoded_ack)
            except (json.JSONDecodeError, RecursionError):
                raise XenoteerError(
                    "invalid_response", "resubscription acknowledgement is invalid"
                ) from None
            ack = decode_server_message(ack_wire)
            if (
                not isinstance(ack, SubscriptionAck)
                or ack.request_id != request_id
                or ack.topics != tuple(subscription["topics"])
            ):
                raise XenoteerError(
                    "invalid_response", "resubscription acknowledgement differed"
                )
            self._subscription = copy.deepcopy(subscription)
            self._active_subscription = copy.deepcopy(subscription)

    async def _heartbeat_loop(self) -> None:
        try:
            while not self._closed:
                await asyncio.sleep(self._heartbeat_interval)
                if self._closed:
                    break
                if time.monotonic() - self._last_received > self._read_stale_timeout:
                    try:
                        await self._socket.close(
                            code=1012, reason="read heartbeat stale"
                        )
                    except Exception:
                        pass
                    continue
                try:
                    await self.send(
                        {
                            "type": "client.ping",
                            "request_id": _new_id(),
                            "nonce": _new_id(),
                        }
                    )
                except XenoteerError:
                    try:
                        await self._socket.close(
                            code=1012, reason="heartbeat send failure"
                        )
                    except Exception:
                        pass
        except asyncio.CancelledError:
            pass

    def __aiter__(self) -> "EventSession":
        return self

    async def __anext__(self) -> EventItem:
        value = await self._queue.get()
        if value is _SENTINEL:
            if self._control_item is not None:
                control_item = self._control_item
                self._control_item = None
                self._queue.put_nowait(_SENTINEL)
                return control_item
            if self._error is not None:
                raise self._error
            raise StopAsyncIteration
        return value  # type: ignore[return-value]

    async def close(self) -> None:
        if (
            self._closed
            and self._reader.done()
            and self._heartbeat.done()
            and self._writer.done()
        ):
            return
        self._closed = True
        current = asyncio.current_task()
        if self._reader is not current and not self._reader.done():
            self._reader.cancel()
        if self._heartbeat is not current and not self._heartbeat.done():
            self._heartbeat.cancel()
        if self._writer is not current and not self._writer.done():
            self._writer.cancel()
        try:
            await self._socket.close(code=1000, reason="client closed")
        except Exception:
            pass
        for task in (self._reader, self._heartbeat, self._writer):
            if task is not current:
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        if self._close_info is None:
            self._close_info = CloseInfo(1000, "client closed", False)

    async def __aenter__(self) -> "EventSession":
        if self._closed:
            raise XenoteerError("transport", "event session is closed")
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.close()

    def __repr__(self) -> str:
        return f"EventSession(url={self._url!r}, authorization='<redacted>')"


def _decode_replay(value: dict[str, Any]) -> ReplayComplete:
    try:
        sequence = decode_uint64(value.get("through_sequence"), allow_zero=True)
    except (TypeError, ValueError):
        raise XenoteerError("invalid_response", "replay sequence is invalid") from None
    desktop_id = value.get("desktop_id")
    generation = value.get("desktop_generation")
    if not isinstance(desktop_id, str) or not isinstance(generation, str):
        raise XenoteerError("invalid_response", "replay scope is invalid")
    return ReplayComplete(desktop_id, generation, sequence, copy.deepcopy(value))


def _decode_resync(value: dict[str, Any]) -> ResyncRequired:
    desktop_id = value.get("desktop_id")
    generation = value.get("desktop_generation")
    reason = value.get("reason")
    if (
        not isinstance(desktop_id, str)
        or not isinstance(generation, str)
        or reason
        not in {
            "generation_changed",
            "history_lost",
            "sequence_ahead",
            "subscriber_lag",
            "outbound_backpressure",
        }
    ):
        raise XenoteerError("invalid_response", "resync barrier is invalid")
    try:
        dropped = decode_uint64(value.get("dropped_through"), allow_zero=True)
        latest = decode_uint64(value.get("latest_sequence"), allow_zero=True)
    except (TypeError, ValueError):
        raise XenoteerError("invalid_response", "resync sequence is invalid") from None
    return ResyncRequired(
        desktop_id, generation, reason, dropped, latest, copy.deepcopy(value)
    )


def _decode_draining(value: dict[str, Any]) -> ServerDraining:
    desktop_id = value.get("desktop_id")
    generation = value.get("desktop_generation")
    reason = value.get("reason_code")
    if (
        not isinstance(desktop_id, str)
        or (generation is not None and not isinstance(generation, str))
        or (reason is not None and not isinstance(reason, str))
    ):
        raise XenoteerError("invalid_response", "server draining message is invalid")
    return ServerDraining(desktop_id, generation, reason, copy.deepcopy(value))


def _decode_server_error(value: dict[str, Any]) -> ServerError:
    code = value.get("code")
    detail = value.get("detail")
    request_id = value.get("request_id")
    generation = value.get("desktop_generation")
    if (
        not isinstance(code, str)
        or not isinstance(detail, str)
        or (request_id is not None and not isinstance(request_id, str))
        or (generation is not None and not isinstance(generation, str))
    ):
        raise XenoteerError("invalid_response", "server error message is invalid")
    return ServerError(
        code, request_id, generation, detail, copy.deepcopy(value)
    )


def _close_info(error: Exception) -> CloseInfo:
    code, reason = _close_code_reason(error)
    return CloseInfo(
        code if isinstance(code, int) else None,
        _safe_close_reason(reason),
        code not in {
            1000,
            1001,
            1002,
            1003,
            1007,
            1008,
            1009,
            1010,
            4401,
            4403,
        },
    )


def _websocket_error(error: Exception, *, connecting: bool = False) -> XenoteerError:
    status = getattr(error, "status_code", None)
    response = getattr(error, "response", None)
    if status is None and response is not None:
        status = getattr(response, "status_code", None)
    code, _ = _close_code_reason(error)
    if status == 401 or code == 4401:
        return XenoteerError("authentication", "WebSocket authentication failed")
    if status == 403 or code in {1008, 4403}:
        return XenoteerError("permission", "WebSocket permission denied")
    return XenoteerError(
        "transport",
        "WebSocket connection failed" if connecting else "WebSocket transport failed",
        source=error,
    )


def _new_id() -> str:
    return str(uuid.uuid4())


def _validate_request_id(value: object) -> None:
    if not isinstance(value, str):
        raise XenoteerError("invalid_request", "request ID is invalid")
    try:
        parsed = uuid.UUID(value)
    except ValueError:
        raise XenoteerError("invalid_request", "request ID is invalid") from None
    if parsed.int == 0:
        raise XenoteerError("invalid_request", "request ID is invalid")


def _close_code_reason(error: Exception) -> tuple[int | None, str | None]:
    received = getattr(error, "rcvd", None)
    code = getattr(received, "code", None)
    reason = getattr(received, "reason", None)
    if not isinstance(code, int):
        code = getattr(error, "status_code", None)
    return (
        code if isinstance(code, int) else None,
        reason if isinstance(reason, str) else None,
    )


def _consume_future_exception(future: asyncio.Future[object]) -> None:
    if not future.cancelled():
        future.exception()


def _safe_close_reason(value: str | None) -> str | None:
    if (
        not isinstance(value, str)
        or not value
        or len(value.encode("utf-8")) > 128
        or any(ord(character) < 0x20 for character in value)
    ):
        return None
    lowered = value.lower()
    if any(
        marker in lowered
        for marker in ("bearer ", "token", "ticket", "authorization", "clipboard")
    ):
        return "<redacted>"
    return value


async def _resolve_authorization(source: AuthorizationSource) -> str:
    try:
        value = source() if callable(source) else source
        if inspect.isawaitable(value):
            value = await value
    except Exception:
        raise XenoteerError(
            "authentication", "WebSocket credential provider failed"
        ) from None
    if (
        not isinstance(value, str)
        or not value.startswith("Bearer ")
        or not 39 <= len(value) <= 1031
        or "\r" in value
        or "\n" in value
    ):
        raise XenoteerError("authentication", "WebSocket credential is invalid")
    return value


def _validate_websocket_url(value: str) -> None:
    try:
        parsed = urlsplit(value)
        parsed.port
    except (TypeError, ValueError):
        raise XenoteerError(
            "invalid_base_url", "WebSocket URL must be a credential-free endpoint"
        ) from None
    if (
        parsed.scheme not in {"ws", "wss"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or parsed.path not in {"", "/", "/v1/ws"}
    ):
        raise XenoteerError(
            "invalid_base_url", "WebSocket URL must be a credential-free endpoint"
        )
    if parsed.scheme == "ws":
        try:
            address = ipaddress.ip_address(parsed.hostname)
        except ValueError:
            raise XenoteerError(
                "invalid_base_url",
                "plaintext WebSocket is permitted only for a numeric loopback address",
            ) from None
        if not address.is_loopback:
            raise XenoteerError(
                "invalid_base_url",
                "plaintext WebSocket is permitted only for a numeric loopback address",
            )


def _decode_welcome(value: object, hello: Mapping[str, Any]) -> ServerWelcome:
    if not isinstance(value, dict):
        raise XenoteerError("invalid_response", "server welcome is invalid")
    if value.get("type") == "error":
        error = _decode_server_error(value)
        raise XenoteerError(
            (
                "authentication"
                if error.code in {"authentication", "invalid_token"}
                else "permission"
                if error.code in {"permission_denied", "forbidden"}
                else error.code
            ),
            "WebSocket hello was rejected",
            request_id=error.request_id,
            problem_code=error.code,
        )
    if value.get("type") != "server.welcome":
        raise XenoteerError(
            "invalid_response", "first server message was not server.welcome"
        )
    protocol = value.get("protocol")
    desktop = value.get("desktop")
    limits = value.get("limits")
    resume = value.get("resume")
    hello_protocol = hello.get("protocol")
    if not all(
        isinstance(item, Mapping)
        for item in (protocol, desktop, limits, resume, hello_protocol)
    ):
        raise XenoteerError("invalid_response", "server welcome is invalid")
    assert isinstance(protocol, Mapping)
    assert isinstance(desktop, Mapping)
    assert isinstance(limits, Mapping)
    assert isinstance(resume, Mapping)
    assert isinstance(hello_protocol, Mapping)
    major = protocol.get("major")
    minor = protocol.get("minor")
    max_message_bytes = limits.get("max_message_bytes")
    heartbeat_ms = limits.get("heartbeat_ms")
    desktop_id = desktop.get("id")
    generation = desktop.get("generation")
    resume_status = resume.get("status")
    if (
        isinstance(major, bool)
        or not isinstance(major, int)
        or isinstance(minor, bool)
        or not isinstance(minor, int)
        or major != hello_protocol.get("major")
        or minor != hello_protocol.get("min_minor")
        or minor != hello_protocol.get("max_minor")
        or not isinstance(desktop_id, str)
        or (generation is not None and not isinstance(generation, str))
        or isinstance(max_message_bytes, bool)
        or not isinstance(max_message_bytes, int)
        or not 1024 <= max_message_bytes <= MAX_WEBSOCKET_MESSAGE_BYTES
        or isinstance(heartbeat_ms, bool)
        or not isinstance(heartbeat_ms, int)
        or not 1000 <= heartbeat_ms <= 300_000
        or resume_status not in {"not_requested", "replayed", "resync_required"}
    ):
        raise XenoteerError("invalid_response", "server welcome is invalid")
    if resume_status == "resync_required":
        raise XenoteerError(
            "resync_required",
            "server could not resume event continuity; fetch authoritative snapshots",
        )
    return ServerWelcome(
        protocol_major=major,
        protocol_minor=minor,
        desktop_id=desktop_id,
        desktop_generation=generation,
        max_message_bytes=max_message_bytes,
        heartbeat_seconds=heartbeat_ms / 1000,
        resume_status=resume_status,
        _raw=copy.deepcopy(value),
    )
