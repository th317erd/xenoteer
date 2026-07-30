# SPDX-License-Identifier: Apache-2.0
"""Shared connection contracts with metadata-only diagnostic logging."""

from __future__ import annotations

import asyncio
import inspect
import math
import re
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any, Literal, Protocol, TypeAlias
from urllib.parse import urlsplit

from .errors import XenoteerError


class WebSocketLike(Protocol):
    """Socket surface owned by the SDK after a factory returns it."""

    async def send(self, message: str) -> Any: ...

    async def recv(self) -> object: ...

    async def close(self, **kwargs: Any) -> Any: ...


WebSocketFactory: TypeAlias = Callable[
    ..., WebSocketLike | Awaitable[WebSocketLike]
]
SafeLogOperation: TypeAlias = Literal[
    "http.request",
    "artifact.upload",
    "artifact.download",
    "artifact.delete",
    "websocket.handshake",
]
SafeLogOutcome: TypeAlias = Literal["started", "succeeded", "failed"]
SafeLogHook: TypeAlias = Callable[["SafeLogEvent"], None]

_OPERATIONS = frozenset(
    {
        "http.request",
        "artifact.upload",
        "artifact.download",
        "artifact.delete",
        "websocket.handshake",
    }
)
_OUTCOMES = frozenset({"started", "succeeded", "failed"})
_METHODS = frozenset({"GET", "POST", "DELETE"})
_SAFE_CODE = re.compile(r"[a-z][a-z0-9_]{0,127}\Z")
_STATIC_ROUTES = frozenset(
    {
        "/v1/status",
        "/v1/capabilities",
        "/v1/ws",
        "/v1/artifacts",
    }
)
_ROUTE_PATTERNS: tuple[tuple[re.Pattern[str], str], ...] = (
    (
        re.compile(r"/v1/artifacts/[^/]+\Z"),
        "/v1/artifacts/{artifact_id}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+\Z"),
        "/v1/desktops/{desktop_id}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/commands\Z"),
        "/v1/desktops/{desktop_id}/commands",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/commands/[^/]+\Z"),
        "/v1/desktops/{desktop_id}/commands/{command_id}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/commands/[^/]+/(?:wait|cancel)\Z"),
        "/v1/desktops/{desktop_id}/commands/{command_id}/{action}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/lease(?:/(?:renew|release))?\Z"),
        "/v1/desktops/{desktop_id}/lease/{action}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/windows\Z"),
        "/v1/desktops/{desktop_id}/windows",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/windows/(?:query|wait|resolve)\Z"),
        "/v1/desktops/{desktop_id}/windows/{action}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/windows/[^/]+(?:/(?:resolve|wait))?\Z"),
        "/v1/desktops/{desktop_id}/windows/{window_ref}/{action}",
    ),
    (
        re.compile(
            r"/v1/desktops/[^/]+/accessibility/(?:query|wait|resolve)\Z"
        ),
        "/v1/desktops/{desktop_id}/accessibility/{action}",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/clipboard/read\Z"),
        "/v1/desktops/{desktop_id}/clipboard/read",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/screenshots\Z"),
        "/v1/desktops/{desktop_id}/screenshots",
    ),
    (
        re.compile(r"/v1/desktops/[^/]+/viewer-tickets\Z"),
        "/v1/desktops/{desktop_id}/viewer-tickets",
    ),
)


@dataclass(frozen=True, slots=True)
class ReconnectPolicy:
    """Validated transport-only reconnect budget and capped jittered delay."""

    max_attempts: int = 5
    initial_delay: float = 0.1
    max_delay: float = 5.0
    jitter_min: float = 0.5
    jitter_max: float = 1.0

    def __post_init__(self) -> None:
        if (
            isinstance(self.max_attempts, bool)
            or not isinstance(self.max_attempts, int)
            or not 0 <= self.max_attempts <= 20
        ):
            raise XenoteerError(
                "invalid_request", "reconnect attempts must be in 0..20"
            )
        values = (
            self.initial_delay,
            self.max_delay,
            self.jitter_min,
            self.jitter_max,
        )
        if any(
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(float(value))
            for value in values
        ):
            raise XenoteerError(
                "invalid_request", "reconnect delay policy is invalid"
            )
        if (
            not 0 <= self.initial_delay <= self.max_delay <= 60
            or not 0 <= self.jitter_min <= self.jitter_max <= 2
        ):
            raise XenoteerError(
                "invalid_request", "reconnect delay policy is invalid"
            )

    def delay_before(self, attempt: int, jitter: float) -> float:
        """Return the exact bounded delay before one one-indexed attempt."""

        if (
            isinstance(attempt, bool)
            or not isinstance(attempt, int)
            or not 1 <= attempt <= self.max_attempts
            or isinstance(jitter, bool)
            or not isinstance(jitter, (int, float))
            or not self.jitter_min <= jitter <= self.jitter_max
        ):
            raise XenoteerError(
                "invalid_request", "reconnect attempt or jitter is invalid"
            )
        exponential = self.initial_delay * (2 ** (attempt - 1))
        return float(min(self.max_delay, exponential) * float(jitter))


@dataclass(frozen=True, slots=True)
class SafeLogEvent:
    """Closed, frozen metadata schema with no raw transport string fields."""

    operation: SafeLogOperation
    outcome: SafeLogOutcome
    attempt: int | None = None
    method: str | None = None
    route: str | None = None
    status: int | None = None
    request_bytes: int | None = None
    response_bytes: int | None = None
    error_code: str | None = None

    def __post_init__(self) -> None:
        if self.operation not in _OPERATIONS or self.outcome not in _OUTCOMES:
            raise XenoteerError("invalid_request", "safe log event is invalid")
        if self.attempt is not None and (
            isinstance(self.attempt, bool)
            or not isinstance(self.attempt, int)
            or not 0 <= self.attempt <= 20
        ):
            raise XenoteerError("invalid_request", "safe log attempt is invalid")
        if self.method is not None and self.method not in _METHODS:
            raise XenoteerError("invalid_request", "safe log method is invalid")
        if self.route is not None and (
            self.route != "unknown"
            and self.route not in _STATIC_ROUTES
            and self.route not in {template for _, template in _ROUTE_PATTERNS}
        ):
            raise XenoteerError("invalid_request", "safe log route is invalid")
        if self.status is not None and (
            isinstance(self.status, bool)
            or not isinstance(self.status, int)
            or not 100 <= self.status <= 599
        ):
            raise XenoteerError("invalid_request", "safe log status is invalid")
        for size in (self.request_bytes, self.response_bytes):
            if size is not None and (
                isinstance(size, bool)
                or not isinstance(size, int)
                or size < 0
            ):
                raise XenoteerError("invalid_request", "safe log size is invalid")
        if self.error_code is not None and (
            not isinstance(self.error_code, str)
            or _SAFE_CODE.fullmatch(self.error_code) is None
        ):
            raise XenoteerError("invalid_request", "safe log error code is invalid")


def classify_safe_route(path: str) -> str:
    """Map a raw origin-relative path to a reviewed identifier-free template."""

    if not isinstance(path, str):
        return "unknown"
    parsed = urlsplit(path)
    route = parsed.path
    if route in _STATIC_ROUTES:
        return route
    for pattern, template in _ROUTE_PATTERNS:
        if pattern.fullmatch(route) is not None:
            return template
    return "unknown"


def safe_error_code(error: BaseException) -> str:
    """Project arbitrary failures onto one stable content-free diagnostic code."""

    if isinstance(error, XenoteerError) and _SAFE_CODE.fullmatch(error.code):
        return error.code
    if isinstance(error, TimeoutError):
        return "request_timeout"
    if isinstance(error, asyncio.CancelledError):
        return "cancelled"
    return "transport"


def validate_safe_log_hook(hook: SafeLogHook | None) -> None:
    """Reject asynchronous hooks before any connection I/O."""

    if hook is None:
        return
    if not callable(hook):
        raise XenoteerError("invalid_request", "safe log hook must be callable")
    call = getattr(hook, "__call__", None)
    if inspect.iscoroutinefunction(hook) or inspect.iscoroutinefunction(call):
        raise XenoteerError(
            "invalid_request", "safe log hook must be synchronous"
        )


def emit_safe_log(hook: SafeLogHook | None, event: SafeLogEvent) -> None:
    """Call a borrowed hook without letting ordinary hook failures affect I/O."""

    if hook is None:
        return
    try:
        outcome = hook(event)
        if inspect.isawaitable(outcome):
            close = getattr(outcome, "close", None)
            if callable(close):
                close()
    except Exception:
        return
