# SPDX-License-Identifier: Apache-2.0
"""Connected client, status validation, and protocol negotiation."""

from __future__ import annotations

import asyncio
import copy
import datetime as dt
import time
import uuid
from collections.abc import Mapping
from dataclasses import dataclass, field
from types import TracebackType
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit

from .desktop import Desktop
from .errors import XenoteerError
from .events import EventSession, WebSocketFactory
from .options import ClientOptions, ProtocolRange
from .protocol_generated import JsonObject
from .state import GenerationRegistry
from .transport import AsyncTransport, HttpTransport
from .wire import as_uint64_string, encode_uint64


_DESKTOP_STATES = frozenset(
    {"booting", "probing", "ready", "degraded", "draining", "stopped", "failed"}
)
_CAPABILITY_STATES = frozenset(
    {"available", "degraded", "unavailable", "disabled"}
)


@dataclass(frozen=True, order=True, slots=True)
class ProtocolVersion:
    major: int
    minor: int

    def __post_init__(self) -> None:
        if (
            isinstance(self.major, bool)
            or isinstance(self.minor, bool)
            or not isinstance(self.major, int)
            or not isinstance(self.minor, int)
            or not 0 <= self.major <= 65_535
            or not 0 <= self.minor <= 65_535
        ):
            raise XenoteerError("invalid_response", "protocol version is invalid")

    def wire(self) -> dict[str, int]:
        return {"major": self.major, "minor": self.minor}


@dataclass(frozen=True, slots=True)
class DesktopStatus:
    id: str
    generation: str | None
    state: str
    reason_code: str | None


@dataclass(frozen=True, slots=True)
class Status:
    server_version: str
    protocol_min: ProtocolVersion
    protocol_max: ProtocolVersion
    server_time: dt.datetime
    desktop: DesktopStatus
    _capabilities: JsonObject = field(repr=False)
    _raw: JsonObject = field(repr=False)

    @property
    def capabilities(self) -> JsonObject:
        return copy.deepcopy(self._capabilities)

    @property
    def raw(self) -> JsonObject:
        return copy.deepcopy(self._raw)


def _version(value: object) -> ProtocolVersion:
    if not isinstance(value, Mapping):
        raise XenoteerError("invalid_response", "protocol version must be an object")
    return ProtocolVersion(value.get("major"), value.get("minor"))  # type: ignore[arg-type]


def _non_nil_uuid(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise XenoteerError("invalid_response", f"{label} is invalid")
    try:
        parsed = uuid.UUID(value)
    except ValueError:
        raise XenoteerError("invalid_response", f"{label} is invalid") from None
    if parsed.int == 0:
        raise XenoteerError("invalid_response", f"{label} is invalid")
    return value


def _timestamp(value: object) -> dt.datetime:
    if not isinstance(value, str):
        raise XenoteerError("invalid_response", "server time is invalid")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        raise XenoteerError("invalid_response", "server time is invalid") from None
    if parsed.tzinfo is None:
        raise XenoteerError("invalid_response", "server time must include an offset")
    return parsed.astimezone(dt.timezone.utc)


def _validate_capabilities(value: object) -> JsonObject:
    if not isinstance(value, dict):
        raise XenoteerError("invalid_response", "capability report is invalid")
    capabilities = value.get("capabilities")
    if not isinstance(capabilities, list) or len(capabilities) > 256:
        raise XenoteerError("invalid_response", "capability report is invalid")
    identifiers: set[str] = set()
    for capability in capabilities:
        if not isinstance(capability, dict):
            raise XenoteerError("invalid_response", "capability entry is invalid")
        identifier = capability.get("id")
        status = capability.get("status")
        reason = capability.get("reason_code")
        backend_version = capability.get("backend_version")
        if (
            not isinstance(identifier, str)
            or not 1 <= len(identifier.encode("utf-8")) <= 128
            or any(not segment for segment in identifier.split("."))
            or any(
                not (
                    "a" <= character <= "z"
                    or "0" <= character <= "9"
                    or character in "._-"
                )
                for character in identifier
            )
            or identifier in identifiers
            or status not in _CAPABILITY_STATES
            or "reason_code" not in capability
            or (
                reason is not None
                and (
                    not isinstance(reason, str)
                    or not 1 <= len(reason.encode("utf-8")) <= 128
                    or any(
                        not (
                            "a" <= character <= "z"
                            or "0" <= character <= "9"
                            or character in "._-"
                        )
                        for character in reason
                    )
                )
            )
            or "backend_version" not in capability
            or (
                backend_version is not None
                and (
                    not isinstance(backend_version, str)
                    or not 1 <= len(backend_version.encode("utf-8")) <= 128
                    or any(
                        ord(character) < 32
                        or 127 <= ord(character) <= 159
                        for character in backend_version
                    )
                )
            )
        ):
            raise XenoteerError("invalid_response", "capability entry is invalid")
        identifiers.add(identifier)
    return value


def validate_status(value: object) -> Status:
    """Validate frozen v1 fields and retain additive response metadata."""

    if not isinstance(value, dict):
        raise XenoteerError("invalid_response", "status response must be an object")
    server_version = value.get("server_version")
    desktop = value.get("desktop")
    capabilities = value.get("capabilities")
    if (
        not isinstance(server_version, str)
        or not server_version
        or len(server_version.encode()) > 128
        or any(not character.isprintable() for character in server_version)
        or not isinstance(desktop, dict)
    ):
        raise XenoteerError("invalid_response", "status response is invalid")
    checked_capabilities = _validate_capabilities(capabilities)
    protocol_min = _version(value.get("protocol_min"))
    protocol_max = _version(value.get("protocol_max"))
    if (
        protocol_min.major != protocol_max.major
        or protocol_min.minor > protocol_max.minor
    ):
        raise XenoteerError("invalid_response", "server protocol range is invalid")
    desktop_id = _non_nil_uuid(desktop.get("id"), "desktop ID")
    generation_value = desktop.get("generation")
    generation = (
        None
        if generation_value is None
        else _non_nil_uuid(generation_value, "desktop generation")
    )
    state = desktop.get("state")
    reason = desktop.get("reason_code")
    if state not in _DESKTOP_STATES or (
        reason is not None
        and (
            not isinstance(reason, str)
            or not 1 <= len(reason.encode()) <= 128
            or any(
                not (
                    "a" <= char <= "z"
                    or "0" <= char <= "9"
                    or char in "._-"
                )
                for char in reason
            )
        )
    ):
        raise XenoteerError("invalid_response", "desktop status is invalid")
    return Status(
        server_version=server_version,
        protocol_min=protocol_min,
        protocol_max=protocol_max,
        server_time=_timestamp(value.get("server_time")),
        desktop=DesktopStatus(desktop_id, generation, state, reason),
        _capabilities=copy.deepcopy(checked_capabilities),
        _raw=copy.deepcopy(value),
    )


def negotiate_protocol(
    client: ProtocolRange,
    server_min: ProtocolVersion,
    server_max: ProtocolVersion,
) -> ProtocolVersion:
    """Select the highest shared minor in one major or fail closed."""

    if server_min.minor > server_max.minor:
        raise XenoteerError("reversed_minor_range", "server protocol range is reversed")
    if server_min.major != server_max.major or client.major != server_min.major:
        raise XenoteerError("unsupported_major", "protocol majors do not overlap")
    minimum = max(client.min_minor, server_min.minor)
    maximum = min(client.max_minor, server_max.minor)
    if minimum > maximum:
        raise XenoteerError("no_shared_minor", "protocol minor ranges do not overlap")
    return ProtocolVersion(client.major, maximum)


def admit_request_version(
    negotiated: ProtocolVersion, request: ProtocolVersion
) -> None:
    """Require the exact post-handshake version on every request."""

    if negotiated != request:
        raise XenoteerError(
            "unsupported_version", "request version differs from negotiated version"
        )


class XenoteerClient:
    """Authenticated, negotiated async SDK root."""

    __slots__ = (
        "_closed",
        "_negotiated",
        "_options",
        "_owned_leases",
        "_registry",
        "_received_monotonic",
        "_round_trip",
        "_sessions",
        "_status",
        "_transport",
        "_close_task",
    )

    def __init__(
        self,
        options: ClientOptions,
        transport: AsyncTransport,
        status: Status,
        negotiated: ProtocolVersion,
        *,
        received_monotonic: float,
        round_trip: float,
    ) -> None:
        self._options = options
        self._transport = transport
        self._status = status
        self._negotiated = negotiated
        self._received_monotonic = received_monotonic
        self._round_trip = round_trip
        self._registry = (
            None
            if status.desktop.generation is None
            else GenerationRegistry(status.desktop.generation)
        )
        self._sessions: list[EventSession] = []
        self._owned_leases: list[ControlLease] = []
        self._closed = False
        self._close_task: asyncio.Task[None] | None = None

    @classmethod
    async def connect(
        cls,
        options: ClientOptions,
        *,
        transport: AsyncTransport | None = None,
    ) -> "XenoteerClient":
        """Authenticate status and negotiate v1 without acquiring control."""

        selected_transport = HttpTransport(options) if transport is None else transport
        started = time.monotonic()
        try:
            status = validate_status(
                await selected_transport.request("GET", "/v1/status")
            )
            received = time.monotonic()
            negotiated = negotiate_protocol(
                options.protocol_range, status.protocol_min, status.protocol_max
            )
        except Exception:
            if transport is None:
                await selected_transport.close()
            raise
        return cls(
            options,
            selected_transport,
            status,
            negotiated,
            received_monotonic=received,
            round_trip=received - started,
        )

    @property
    def status(self) -> Status:
        return self._status

    @property
    def negotiated_protocol(self) -> ProtocolVersion:
        return self._negotiated

    def desktop(self) -> Desktop:
        self._ensure_open()
        generation = self._status.desktop.generation
        if generation is None:
            raise XenoteerError(
                "desktop_unavailable", "desktop session is not currently available"
            )
        return Desktop(
            self._transport,
            self._status.desktop.id,
            generation,
            self._negotiated.wire(),
            registry=self._registry,
            owned_leases=self._owned_leases,
        )

    async def capabilities(self) -> JsonObject:
        """Refresh the authenticated additive capability report."""

        self._ensure_open()
        response = await self._transport.request("GET", "/v1/capabilities")
        return copy.deepcopy(_validate_capabilities(response))

    def deadline_after(self, duration: float) -> str:
        """Derive a conservative server deadline from status time and monotonic age."""

        if (
            isinstance(duration, bool)
            or not isinstance(duration, (int, float))
            or not 0 < duration <= 3600
        ):
            raise XenoteerError("invalid_request", "deadline duration is invalid")
        age = time.monotonic() - self._received_monotonic
        deadline = self._status.server_time + dt.timedelta(
            seconds=age + self._round_trip + duration
        )
        return deadline.isoformat().replace("+00:00", "Z")

    async def open_events(
        self,
        *,
        capacity: int = 256,
        resume: Mapping[str, Any] | None = None,
        websocket_factory: WebSocketFactory | None = None,
    ) -> EventSession:
        """Open the header-authenticated control WebSocket; token never enters URL."""

        self._ensure_open()
        resume_wire: dict[str, Any] | None = None
        if resume is not None:
            sequence = resume.get("event_sequence")
            if isinstance(sequence, int) and not isinstance(sequence, bool):
                sequence = encode_uint64(sequence, allow_zero=True)
            else:
                sequence = as_uint64_string(sequence, allow_zero=True)
            resume_wire = {
                "desktop_id": resume.get("desktop_id"),
                "desktop_generation": resume.get("desktop_generation"),
                "event_sequence": sequence,
            }
        hello = {
            "type": "client.hello",
            "request_id": str(uuid.uuid4()),
            "protocol": {
                "major": self._negotiated.major,
                "min_minor": self._negotiated.minor,
                "max_minor": self._negotiated.minor,
            },
            "client": {
                "name": self._options.client_name,
                "version": self._options.client_version,
            },
            "resume": resume_wire,
        }
        parsed = urlsplit(self._transport.base_url)
        websocket_url = (
            f"{'wss' if parsed.scheme == 'https' else 'ws'}://{parsed.netloc}/v1/ws"
        )
        session = await EventSession.connect(
            websocket_url,
            self._transport.authorization_header,
            hello,
            capacity=capacity,
            websocket_factory=websocket_factory,
            heartbeat_interval=self._options.heartbeat_interval,
            read_stale_timeout=(
                self._options.read_stale_timeout
                if self._options.read_stale_timeout is not None
                else min(
                    600.0,
                    max(
                        self._options.heartbeat_interval * 3,
                        self._options.heartbeat_interval + 1,
                    ),
                )
            ),
            max_reconnect_attempts=self._options.max_reconnect_attempts,
            registry=self._registry,
        )
        if (
            session.welcome.desktop_id != self._status.desktop.id
            or session.welcome.desktop_generation
            != self._status.desktop.generation
        ):
            if self._registry is not None:
                self._registry.invalidate("websocket_status_scope_changed")
            await session.close()
            raise XenoteerError(
                "generation_changed",
                "WebSocket desktop scope differs from authenticated status",
            )
        self._sessions.append(session)
        return session

    async def close(self) -> None:
        if self._close_task is None:
            self._closed = True
            self._close_task = asyncio.create_task(
                self._close_impl(), name="xenoteer-client-close"
            )
        try:
            await asyncio.shield(self._close_task)
        except asyncio.CancelledError:
            self._close_task.add_done_callback(_consume_close_failure)
            raise

    async def _close_impl(self) -> None:
        sessions = list(self._sessions)
        leases = [lease for lease in self._owned_leases if lease.requires_cleanup]
        self._sessions.clear()
        self._owned_leases.clear()
        cleanup_tasks: list[asyncio.Task[Any]] = [
            asyncio.create_task(session.close(), name="xenoteer-event-close")
            for session in sessions
        ]
        cleanup_tasks.extend(
            asyncio.create_task(lease.release(), name="xenoteer-lease-release")
            for lease in leases
        )
        failures: list[BaseException] = []
        if cleanup_tasks:
            done, pending = await asyncio.wait(
                cleanup_tasks,
                timeout=min(5.0, float(self._options.request_timeout)),
            )
            if pending:
                failures.append(
                    TimeoutError(
                        f"{len(pending)} client-owned resource cleanup task(s) timed out"
                    )
                )
                for task in pending:
                    task.cancel()
            results = await asyncio.gather(*cleanup_tasks, return_exceptions=True)
            for result in results:
                if isinstance(result, XenoteerError) and result.code == "lease_released":
                    continue
                if isinstance(result, BaseException) and not isinstance(
                    result, asyncio.CancelledError
                ):
                    failures.append(result)
        try:
            await asyncio.wait_for(
                self._transport.close(),
                timeout=min(5.0, float(self._options.request_timeout)),
            )
        except BaseException as error:
            if isinstance(error, asyncio.CancelledError):
                raise
            failures.append(error)
        if failures:
            raise XenoteerError(
                "cleanup_failed",
                f"{len(failures)} client resource cleanup operation(s) failed",
                source=failures[0],
            )

    async def __aenter__(self) -> "XenoteerClient":
        self._ensure_open()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        try:
            await self.close()
        except Exception as error:
            if exc is None:
                raise
            exc.add_note(f"Xenoteer client cleanup failed: {type(error).__name__}")

    def _ensure_open(self) -> None:
        if self._closed:
            raise XenoteerError("transport", "Xenoteer client is closed")

    def __repr__(self) -> str:
        return (
            f"XenoteerClient(options={dict(self._options.safe_dict())!r}, "
            f"protocol={self._negotiated!r}, closed={self._closed})"
        )


def _consume_close_failure(task: asyncio.Task[None]) -> None:
    if not task.cancelled():
        task.exception()


if TYPE_CHECKING:
    from .lease import ControlLease
