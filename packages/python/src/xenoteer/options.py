# SPDX-License-Identifier: Apache-2.0
"""Connection options with fail-closed credential redaction."""

from __future__ import annotations

import inspect
import ipaddress
import re
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from types import MappingProxyType
from typing import TypeAlias
from urllib.parse import urlsplit

from .errors import XenoteerError


TokenProvider: TypeAlias = Callable[[], str | Awaitable[str]]
TokenSource: TypeAlias = str | TokenProvider
_TOKEN68 = re.compile(r"[A-Za-z0-9._~+/-]+={0,}\Z")


class BearerToken:
    """Opaque bearer credential whose repr and str are always redacted."""

    __slots__ = ("__value",)

    def __init__(self, value: str) -> None:
        if (
            not isinstance(value, str)
            or not 32 <= len(value) <= 1024
            or _TOKEN68.fullmatch(value) is None
            or re.search(r"=[^=]", value) is not None
        ):
            raise XenoteerError("invalid_token", "invalid Xenoteer bearer token")
        self.__value = value

    def authorization_header(self) -> str:
        """Return the credential only at the HTTP authentication boundary."""

        return f"Bearer {self.__value}"

    def __repr__(self) -> str:
        return "BearerToken(<redacted>)"

    __str__ = __repr__


@dataclass(frozen=True, slots=True)
class ProtocolRange:
    """Inclusive supported minor range within one protocol major."""

    major: int = 1
    min_minor: int = 0
    max_minor: int = 0

    def __post_init__(self) -> None:
        values = (self.major, self.min_minor, self.max_minor)
        if (
            all(isinstance(value, int) and not isinstance(value, bool) for value in values)
            and self.min_minor > self.max_minor
        ):
            raise XenoteerError(
                "reversed_minor_range", "client protocol range is reversed"
            )
        if (
            any(isinstance(value, bool) or not isinstance(value, int) for value in values)
            or any(value < 0 or value > 65_535 for value in values)
        ):
            raise XenoteerError("invalid_request", "client protocol range is invalid")


@dataclass(frozen=True, slots=True)
class ClientOptions:
    """Validated client configuration whose diagnostics cannot reveal a token."""

    base_url: str
    token: TokenSource = field(repr=False)
    request_timeout: float = 35.0
    max_response_bytes: int = 1_048_576
    client_name: str = "xenoteer"
    client_version: str = "0.1.0"
    protocol_range: ProtocolRange = field(default_factory=ProtocolRange)
    heartbeat_interval: float = 15.0
    read_stale_timeout: float | None = None
    max_reconnect_attempts: int = 5

    def __post_init__(self) -> None:
        try:
            parsed = urlsplit(self.base_url)
            parsed.port
        except (TypeError, ValueError):
            raise XenoteerError(
                "invalid_base_url", "base URL must be an absolute HTTP(S) origin"
            ) from None
        if (
            parsed.scheme not in {"http", "https"}
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.path not in {"", "/"}
            or parsed.query
            or parsed.fragment
        ):
            raise XenoteerError(
                "invalid_base_url",
                "base URL must be an HTTP(S) origin without credentials, path, query, or fragment",
            )
        if parsed.scheme == "http":
            try:
                address = ipaddress.ip_address(parsed.hostname)
            except ValueError:
                raise XenoteerError(
                    "invalid_base_url",
                    "plaintext HTTP is permitted only for a numeric loopback address",
                ) from None
            if not address.is_loopback:
                raise XenoteerError(
                    "invalid_base_url",
                    "plaintext HTTP is permitted only for a numeric loopback address",
                )
        if (
            isinstance(self.request_timeout, bool)
            or not isinstance(self.request_timeout, (int, float))
            or not 0 < self.request_timeout <= 300
        ):
            raise XenoteerError("invalid_request", "request timeout must be in (0, 300] seconds")
        if (
            isinstance(self.max_response_bytes, bool)
            or not isinstance(self.max_response_bytes, int)
            or not 1 <= self.max_response_bytes <= 32 * 1_048_576
        ):
            raise XenoteerError("invalid_request", "response limit is outside its supported range")
        for label, value in (
            ("client name", self.client_name),
            ("client version", self.client_version),
        ):
            if not isinstance(value, str) or not value or len(value.encode()) > 128:
                raise XenoteerError("invalid_request", f"{label} is invalid")
        if (
            isinstance(self.heartbeat_interval, bool)
            or not isinstance(self.heartbeat_interval, (int, float))
            or not 1 <= self.heartbeat_interval <= 300
        ):
            raise XenoteerError(
                "invalid_request", "heartbeat interval must be in [1, 300] seconds"
            )
        if self.read_stale_timeout is not None and (
            isinstance(self.read_stale_timeout, bool)
            or not isinstance(self.read_stale_timeout, (int, float))
            or not self.heartbeat_interval < self.read_stale_timeout <= 600
        ):
            raise XenoteerError(
                "invalid_request",
                "read stale timeout must exceed heartbeat and be at most 600 seconds",
            )
        if (
            isinstance(self.max_reconnect_attempts, bool)
            or not isinstance(self.max_reconnect_attempts, int)
            or not 0 <= self.max_reconnect_attempts <= 20
        ):
            raise XenoteerError(
                "invalid_request", "max reconnect attempts must be in 0..20"
            )

    def safe_dict(self) -> MappingProxyType[str, object]:
        """Return the only supported diagnostics projection."""

        return MappingProxyType(
            {
                "base_url": self.base_url,
                "token": "<redacted>",
                "request_timeout": self.request_timeout,
                "max_response_bytes": self.max_response_bytes,
                "client_name": self.client_name,
                "client_version": self.client_version,
                "protocol_range": self.protocol_range,
                "heartbeat_interval": self.heartbeat_interval,
                "read_stale_timeout": self.read_stale_timeout,
                "max_reconnect_attempts": self.max_reconnect_attempts,
            }
        )

    def __repr__(self) -> str:
        return f"ClientOptions({dict(self.safe_dict())!r})"


async def resolve_token(source: TokenSource) -> BearerToken:
    """Resolve a sync/async provider without retaining provider failures."""

    try:
        value = source() if callable(source) else source
        if inspect.isawaitable(value):
            value = await value
    except Exception:
        raise XenoteerError("invalid_token", "Xenoteer token provider failed") from None
    if not isinstance(value, str):
        raise XenoteerError("invalid_token", "Xenoteer token provider returned a non-string")
    return BearerToken(value)
