# SPDX-License-Identifier: Apache-2.0
"""Bounded, retry-neutral HTTP transport for the public v1 API."""

from __future__ import annotations

import asyncio
import hashlib
import inspect
import ipaddress
import json
import math
from collections.abc import AsyncIterable, Awaitable, Callable, Mapping
from typing import Any, Protocol, runtime_checkable
from urllib.parse import urlsplit

from .connection import (
    SafeLogEvent,
    SafeLogHook,
    SafeLogOperation,
    classify_safe_route,
    emit_safe_log,
    safe_error_code,
)
from .errors import XenoteerError, error_from_problem
from .options import ClientOptions, resolve_token
from .protocol_generated import JsonObject


@runtime_checkable
class AsyncTransport(Protocol):
    """Narrow injectable transport used by all SDK domain objects."""

    @property
    def base_url(self) -> str: ...

    async def authorization_header(self) -> str: ...

    async def request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
    ) -> JsonObject: ...

    async def upload_artifact(self, path: str, content_type: str, body: bytes) -> JsonObject: ...

    async def upload_artifact_stream(
        self,
        path: str,
        content_type: str,
        body: AsyncIterable[bytes],
        *,
        content_length: int,
        sha256: str,
    ) -> JsonObject: ...

    async def download_artifact(
        self,
        path: str,
        artifact: object,
        sink: Callable[[bytes], Awaitable[None]],
    ) -> None: ...

    async def delete_artifact(self, path: str) -> None: ...

    async def close(self) -> None: ...


@runtime_checkable
class AsyncDeadlineTransport(Protocol):
    """Optional additive capability for one exact per-request HTTP deadline."""

    async def request_with_timeout(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
        timeout: float,
    ) -> JsonObject: ...


def _validated_request_timeout(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise XenoteerError(
            "invalid_request",
            "per-request timeout must be greater than zero and at most 305 seconds",
        )
    try:
        timeout = float(value)
    except (OverflowError, TypeError, ValueError):
        timeout = math.nan
    if not math.isfinite(timeout) or not 0 < timeout <= 305:
        raise XenoteerError(
            "invalid_request",
            "per-request timeout must be greater than zero and at most 305 seconds",
        )
    return timeout


async def request_with_deadline(
    transport: AsyncTransport,
    method: str,
    path: str,
    body: Mapping[str, Any] | None = None,
    *,
    headers: Mapping[str, str] | None = None,
    timeout: float,
) -> JsonObject:
    """Use an exact transport deadline when supported, otherwise bound it locally."""

    timeout = _validated_request_timeout(timeout)
    try:
        async with asyncio.timeout(timeout):
            if isinstance(transport, AsyncDeadlineTransport):
                return await transport.request_with_timeout(
                    method,
                    path,
                    body,
                    headers=headers,
                    timeout=timeout,
                )
            return await transport.request(method, path, body, headers=headers)
    except TimeoutError:
        raise XenoteerError("request_timeout", "Xenoteer request timed out") from None


def normalize_base_url(value: str) -> str:
    """Require an HTTP(S) origin without credentials or path state."""

    try:
        parsed = urlsplit(value)
        port = parsed.port
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
    host = parsed.hostname
    if ":" in host:
        host = f"[{host}]"
    authority = host if port is None else f"{host}:{port}"
    return f"{parsed.scheme}://{authority}"


class HttpTransport:
    """One-attempt HTTP transport with bounded response collection."""

    __slots__ = (
        "_active_operations",
        "_base_url",
        "_client",
        "_closed",
        "_max_response_bytes",
        "_options",
        "_owns_client",
    )
    _client: Any

    def __init__(self, options: ClientOptions, *, http_client: Any | None = None) -> None:
        self._options = options
        self._base_url = normalize_base_url(options.base_url)
        self._max_response_bytes = options.max_response_bytes
        self._active_operations: set[asyncio.Task[Any]] = set()
        self._closed = False
        if http_client is None:
            try:
                import httpx
            except ImportError:
                raise XenoteerError(
                    "missing_dependency", "httpx is required for HTTP connections"
                ) from None
            self._client = httpx.AsyncClient(
                base_url=self._base_url,
                timeout=float(options.request_timeout),
                follow_redirects=False,
            )
            self._owns_client = True
        else:
            self._client = http_client
            self._owns_client = False

    @property
    def base_url(self) -> str:
        return self._base_url

    async def authorization_header(self) -> str:
        async def resolve(_deadline: float) -> str:
            return await self._authorization_header()

        return await self._execute_unlogged(
            float(self._options.request_timeout),
            resolve,
        )

    async def _authorization_header(self) -> str:
        token = await resolve_token(self._options.token)
        return token.authorization_header()

    async def _execute_unlogged(
        self,
        timeout: float,
        operation: Callable[[float], Awaitable[Any]],
    ) -> Any:
        if self._closed:
            raise XenoteerError("transport", "HTTP transport is closed")
        task = asyncio.current_task()
        if task is None:
            raise RuntimeError("HTTP operation has no asyncio task")
        self._active_operations.add(task)
        deadline = asyncio.get_running_loop().time() + timeout
        try:
            async with asyncio.timeout_at(deadline):
                return await operation(deadline)
        except TimeoutError:
            raise XenoteerError(
                "request_timeout", "Xenoteer request timed out"
            ) from None
        finally:
            self._active_operations.discard(task)

    async def _execute_attempt(
        self,
        attempt: "_SafeAttempt",
        timeout: float,
        operation: Callable[[float], Awaitable[Any]],
    ) -> Any:
        try:
            return await self._execute_unlogged(timeout, operation)
        except BaseException as caught:
            failure = _transport_failure(caught)
            attempt.failed(failure)
            if failure is caught:
                raise
            raise failure from None

    async def request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
    ) -> JsonObject:
        """Perform exactly one exchange; mutations are never replayed."""

        return await self._run_request(
            method,
            path,
            body,
            headers=headers,
            timeout=float(self._options.request_timeout),
        )

    async def request_with_timeout(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
        timeout: float,
    ) -> JsonObject:
        """Perform one exchange with an exact bounded override."""

        timeout = _validated_request_timeout(timeout)
        return await self._run_request(
            method,
            path,
            body,
            headers=headers,
            timeout=timeout,
        )

    async def _run_request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None,
        *,
        headers: Mapping[str, str] | None,
        timeout: float,
    ) -> JsonObject:
        method = method.upper()
        if method not in {"GET", "POST", "DELETE"}:
            raise XenoteerError("invalid_request", "unsupported HTTP method")
        if not path.startswith("/") or path.startswith("//"):
            raise XenoteerError("invalid_request", "request path must be origin-relative")

        attempt = _SafeAttempt(
            self._options.safe_log_hook,
            "http.request",
            method=method,
            path=path,
        )

        async def operation(deadline: float) -> JsonObject:
            return await self._request(
                method,
                path,
                body,
                headers=headers,
                deadline=deadline,
                adapter_timeout=timeout,
                attempt=attempt,
            )

        return await self._execute_attempt(attempt, timeout, operation)

    async def _request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None,
        *,
        headers: Mapping[str, str] | None,
        deadline: float,
        adapter_timeout: float,
        attempt: "_SafeAttempt",
    ) -> JsonObject:
        del deadline
        encoded: bytes | None = None
        if body is not None:
            try:
                encoded = json.dumps(
                    body,
                    ensure_ascii=False,
                    allow_nan=False,
                    separators=(",", ":"),
                    sort_keys=True,
                ).encode("utf-8")
            except (TypeError, ValueError):
                raise XenoteerError(
                    "invalid_request", "request is not valid JSON"
                ) from None
            if len(encoded) > self._max_response_bytes:
                raise XenoteerError(
                    "invalid_request", "request body exceeds SDK limit"
                )
        request_headers = {
            "accept": "application/json, application/problem+json",
            **({} if headers is None else dict(headers)),
            "authorization": await self._authorization_header(),
        }
        if encoded is not None:
            request_headers["content-type"] = "application/json"
        async with self._client.stream(
            method,
            path,
            headers=request_headers,
            content=encoded,
            timeout=adapter_timeout,
        ) as response:
            response_status = int(response.status_code)
            content = await _collect_bounded(response, self._max_response_bytes)
            response_bytes = len(content)
            decoded = _decode_json_response(response, content)
        attempt.succeeded(
            status=response_status,
            request_bytes=0 if encoded is None else len(encoded),
            response_bytes=response_bytes,
        )
        return decoded

    async def upload_artifact(self, path: str, content_type: str, body: bytes) -> JsonObject:
        """Upload one already-bounded immutable clipboard-input body."""

        if not body:
            raise XenoteerError("invalid_request", "artifact body must not be empty")
        digest = hashlib.sha256(body).hexdigest()

        async def chunks() -> AsyncIterable[bytes]:
            yield body

        return await self.upload_artifact_stream(
            path,
            content_type,
            chunks(),
            content_length=len(body),
            sha256=digest,
        )

    async def upload_artifact_stream(
        self,
        path: str,
        content_type: str,
        body: AsyncIterable[bytes],
        *,
        content_length: int,
        sha256: str,
    ) -> JsonObject:
        """Stream one bounded body while validating exact length and digest."""

        if (
            isinstance(content_length, bool)
            or not isinstance(content_length, int)
            or not 1 <= content_length <= 32 * 1_024 * 1_024
            or not isinstance(sha256, str)
            or len(sha256) != 64
            or any(character not in "0123456789abcdef" for character in sha256)
        ):
            raise XenoteerError("invalid_request", "artifact stream metadata is invalid")

        attempt = _SafeAttempt(
            self._options.safe_log_hook,
            "artifact.upload",
            method="POST",
            path=path,
        )

        async def operation(deadline: float) -> JsonObject:
            return await self._upload_artifact_stream(
                path,
                content_type,
                body,
                content_length=content_length,
                sha256=sha256,
                deadline=deadline,
                adapter_timeout=float(self._options.request_timeout),
                attempt=attempt,
            )

        return await self._execute_attempt(
            attempt,
            float(self._options.request_timeout),
            operation,
        )

    async def _upload_artifact_stream(
        self,
        path: str,
        content_type: str,
        body: AsyncIterable[bytes],
        *,
        content_length: int,
        sha256: str,
        deadline: float,
        adapter_timeout: float,
        attempt: "_SafeAttempt",
    ) -> JsonObject:
        del deadline

        async def checked_chunks() -> AsyncIterable[bytes]:
            received = 0
            digest = hashlib.sha256()
            try:
                async for chunk in body:
                    if not isinstance(chunk, bytes):
                        raise XenoteerError(
                            "invalid_request", "artifact stream chunks must be bytes"
                        )
                    received += len(chunk)
                    if received > content_length:
                        raise XenoteerError(
                            "request_too_large",
                            f"artifact stream exceeds {content_length} bytes",
                        )
                    digest.update(chunk)
                    if chunk:
                        yield chunk
            except asyncio.CancelledError:
                raise
            except XenoteerError:
                raise
            except Exception as error:
                raise XenoteerError(
                    "artifact_input",
                    "artifact source failed",
                    source=error,
                ) from None
            if received != content_length:
                raise XenoteerError(
                    "invalid_request",
                    f"artifact stream ended after {received} of {content_length} bytes",
                )
            if digest.hexdigest() != sha256:
                raise XenoteerError("invalid_request", "artifact stream digest did not match")

        headers = {
            "authorization": await self._authorization_header(),
            "accept": "application/json, application/problem+json",
            "content-type": content_type,
            "content-length": str(content_length),
            "x-content-sha256": sha256,
        }
        async with self._client.stream(
            "POST",
            path,
            headers=headers,
            content=checked_chunks(),
            timeout=adapter_timeout,
        ) as response:
            response_status = int(response.status_code)
            content = await _collect_bounded(response, self._max_response_bytes)
            response_bytes = len(content)
            decoded = _decode_json_response(response, content)
        if (
            decoded.get("content_type") != content_type
            or decoded.get("content_length") != content_length
            or decoded.get("sha256") != sha256
        ):
            raise XenoteerError(
                "invalid_response",
                "uploaded artifact metadata does not match its body",
            )
        attempt.succeeded(
            status=response_status,
            request_bytes=content_length,
            response_bytes=response_bytes,
        )
        return decoded

    async def download_artifact(
        self,
        path: str,
        artifact: object,
        sink: Callable[[bytes], Awaitable[None]],
    ) -> None:
        """Stream a complete artifact, enforcing metadata before writing."""

        expected_length = getattr(artifact, "content_length", None)
        content_type = getattr(artifact, "content_type", None)
        expected_digest = getattr(artifact, "sha256", None)
        if (
            isinstance(expected_length, bool)
            or not isinstance(expected_length, int)
            or not 1 <= expected_length <= 32 * 1_024 * 1_024
            or not isinstance(content_type, str)
            or not isinstance(expected_digest, str)
        ):
            raise XenoteerError("invalid_request", "artifact reference is invalid")
        if not _is_async_callable(sink):
            raise XenoteerError(
                "invalid_request",
                "artifact sink must be an async callable",
            )
        attempt = _SafeAttempt(
            self._options.safe_log_hook,
            "artifact.download",
            method="GET",
            path=path,
        )

        async def operation(deadline: float) -> None:
            received = 0
            headers = {
                "authorization": await self._authorization_header(),
                "accept": content_type,
            }
            async with self._client.stream(
                "GET",
                path,
                headers=headers,
                timeout=float(self._options.request_timeout),
            ) as response:
                response_status = int(response.status_code)
                if response_status != 200:
                    content = await _collect_bounded(
                        response, self._max_response_bytes
                    )
                    received = len(content)
                    _raise_http_error(response, content)
                _require_exact_header(
                    response.headers, "content-length", str(expected_length)
                )
                _require_exact_header(
                    response.headers, "content-type", content_type
                )
                _require_exact_header(
                    response.headers, "x-content-sha256", expected_digest
                )
                if response.headers.get("content-range") is not None:
                    raise XenoteerError(
                        "invalid_response",
                        "artifact response must not be ranged",
                    )
                digest = hashlib.sha256()
                async for chunk in response.aiter_bytes():
                    received += len(chunk)
                    if received > expected_length:
                        raise XenoteerError(
                            "response_too_large",
                            f"artifact exceeds {expected_length} bytes",
                        )
                    digest.update(chunk)
                    await sink(bytes(chunk))
                if (
                    received != expected_length
                    or digest.hexdigest() != expected_digest
                ):
                    raise XenoteerError(
                        "invalid_response",
                        "artifact length or digest did not match its reference",
                    )
            attempt.succeeded(
                status=response_status,
                response_bytes=received,
            )

        await self._execute_attempt(
            attempt,
            float(self._options.request_timeout),
            operation,
        )

    async def delete_artifact(self, path: str) -> None:
        """Delete one exact scoped artifact and require an empty 204."""

        attempt = _SafeAttempt(
            self._options.safe_log_hook,
            "artifact.delete",
            method="DELETE",
            path=path,
        )

        async def operation(deadline: float) -> None:
            headers = {
                "authorization": await self._authorization_header(),
                "accept": "application/problem+json",
            }
            async with self._client.stream(
                "DELETE",
                path,
                headers=headers,
                timeout=float(self._options.request_timeout),
            ) as response:
                response_status = int(response.status_code)
                content = await _collect_bounded(response, self._max_response_bytes)
                response_bytes = len(content)
                if response_status != 204:
                    _raise_http_error(response, content)
                declared = response.headers.get("content-length")
                if declared not in {None, "0"} or content:
                    raise XenoteerError(
                        "invalid_response", "artifact deletion response was not empty"
                    )
            attempt.succeeded(
                status=response_status,
                request_bytes=0,
                response_bytes=response_bytes,
            )

        await self._execute_attempt(
            attempt,
            float(self._options.request_timeout),
            operation,
        )

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        current = asyncio.current_task()
        active = [
            task
            for task in self._active_operations
            if task is not current and not task.done()
        ]
        for task in active:
            task.cancel()
        if active:
            await asyncio.gather(*active, return_exceptions=True)
        if self._owns_client:
            async with asyncio.timeout(float(self._options.request_timeout)):
                await self._client.aclose()

    def __repr__(self) -> str:
        return f"HttpTransport(base_url={self._base_url!r}, token='<redacted>')"


def with_safe_logging(
    transport: AsyncTransport,
    hook: SafeLogHook | None,
) -> AsyncTransport:
    """Wrap a custom one-attempt adapter without changing its ownership."""

    if hook is None or isinstance(transport, HttpTransport):
        return transport
    return _SafeLoggingTransport(transport, hook)


class _SafeLoggingTransport:
    """Metadata-only logging boundary for an injected transport adapter."""

    __slots__ = ("_hook", "_transport")

    def __init__(self, transport: AsyncTransport, hook: SafeLogHook) -> None:
        self._transport = transport
        self._hook = hook

    @property
    def base_url(self) -> str:
        return self._transport.base_url

    async def authorization_header(self) -> str:
        return await self._transport.authorization_header()

    async def request(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
    ) -> JsonObject:
        attempt = _SafeAttempt(
            self._hook,
            "http.request",
            method=method.upper(),
            path=path,
        )
        request_bytes = _json_size(body)
        try:
            result = await self._transport.request(
                method, path, body, headers=headers
            )
        except BaseException as error:
            attempt.failed(error, request_bytes=request_bytes)
            raise
        attempt.succeeded(
            request_bytes=request_bytes,
            response_bytes=_json_size(result),
        )
        return result

    async def request_with_timeout(
        self,
        method: str,
        path: str,
        body: Mapping[str, Any] | None = None,
        *,
        headers: Mapping[str, str] | None = None,
        timeout: float,
    ) -> JsonObject:
        timeout = _validated_request_timeout(timeout)
        attempt = _SafeAttempt(
            self._hook,
            "http.request",
            method=method.upper(),
            path=path,
        )
        request_bytes = _json_size(body)
        try:
            async with asyncio.timeout(timeout):
                if isinstance(self._transport, AsyncDeadlineTransport):
                    result = await self._transport.request_with_timeout(
                        method,
                        path,
                        body,
                        headers=headers,
                        timeout=timeout,
                    )
                else:
                    result = await self._transport.request(
                        method, path, body, headers=headers
                    )
        except BaseException as error:
            attempt.failed(error, request_bytes=request_bytes)
            raise
        attempt.succeeded(
            request_bytes=request_bytes,
            response_bytes=_json_size(result),
        )
        return result

    async def upload_artifact(
        self, path: str, content_type: str, body: bytes
    ) -> JsonObject:
        attempt = _SafeAttempt(
            self._hook,
            "artifact.upload",
            method="POST",
            path=path,
        )
        try:
            result = await self._transport.upload_artifact(
                path, content_type, body
            )
        except BaseException as error:
            attempt.failed(error, request_bytes=len(body))
            raise
        attempt.succeeded(
            request_bytes=len(body),
            response_bytes=_json_size(result),
        )
        return result

    async def upload_artifact_stream(
        self,
        path: str,
        content_type: str,
        body: AsyncIterable[bytes],
        *,
        content_length: int,
        sha256: str,
    ) -> JsonObject:
        attempt = _SafeAttempt(
            self._hook,
            "artifact.upload",
            method="POST",
            path=path,
        )
        try:
            result = await self._transport.upload_artifact_stream(
                path,
                content_type,
                body,
                content_length=content_length,
                sha256=sha256,
            )
        except BaseException as error:
            attempt.failed(error, request_bytes=content_length)
            raise
        attempt.succeeded(
            request_bytes=content_length,
            response_bytes=_json_size(result),
        )
        return result

    async def download_artifact(
        self,
        path: str,
        artifact: object,
        sink: Callable[[bytes], Awaitable[None]],
    ) -> None:
        if not _is_async_callable(sink):
            raise XenoteerError(
                "invalid_request",
                "artifact sink must be an async callable",
            )
        attempt = _SafeAttempt(
            self._hook,
            "artifact.download",
            method="GET",
            path=path,
        )
        response_bytes = 0

        async def counted_sink(chunk: bytes) -> None:
            nonlocal response_bytes
            await sink(chunk)
            response_bytes += len(chunk)

        try:
            await self._transport.download_artifact(
                path, artifact, counted_sink
            )
        except BaseException as error:
            attempt.failed(error, response_bytes=response_bytes)
            raise
        attempt.succeeded(response_bytes=response_bytes)

    async def delete_artifact(self, path: str) -> None:
        attempt = _SafeAttempt(
            self._hook,
            "artifact.delete",
            method="DELETE",
            path=path,
        )
        try:
            await self._transport.delete_artifact(path)
        except BaseException as error:
            attempt.failed(error, request_bytes=0)
            raise
        attempt.succeeded(request_bytes=0, response_bytes=0)

    async def close(self) -> None:
        await self._transport.close()


class _SafeAttempt:
    """Emit one started event and at most one terminal event."""

    __slots__ = ("_hook", "_method", "_operation", "_route", "_terminal")

    def __init__(
        self,
        hook: SafeLogHook | None,
        operation: SafeLogOperation,
        *,
        method: str,
        path: str,
    ) -> None:
        self._hook = hook
        self._operation = operation
        self._method = method
        self._route = classify_safe_route(path)
        self._terminal = False
        emit_safe_log(
            hook,
            SafeLogEvent(
                operation=operation,
                outcome="started",
                method=method,
                route=self._route,
            ),
        )

    def succeeded(
        self,
        *,
        status: int | None = None,
        request_bytes: int | None = None,
        response_bytes: int | None = None,
    ) -> None:
        if self._terminal:
            return
        self._terminal = True
        emit_safe_log(
            self._hook,
            SafeLogEvent(
                operation=self._operation,
                outcome="succeeded",
                method=self._method,
                route=self._route,
                status=status,
                request_bytes=request_bytes,
                response_bytes=response_bytes,
            ),
        )

    def failed(
        self,
        error: BaseException,
        *,
        status: int | None = None,
        request_bytes: int | None = None,
        response_bytes: int | None = None,
    ) -> None:
        if self._terminal:
            return
        self._terminal = True
        emit_safe_log(
            self._hook,
            SafeLogEvent(
                operation=self._operation,
                outcome="failed",
                method=self._method,
                route=self._route,
                status=status,
                request_bytes=request_bytes,
                response_bytes=response_bytes,
                error_code=safe_error_code(error),
            ),
        )


async def _collect_bounded(response: Any, maximum: int) -> bytes:
    declared = response.headers.get("content-length")
    if declared is not None:
        try:
            length = int(declared, 10)
        except ValueError:
            raise XenoteerError("invalid_response", "invalid Content-Length") from None
        if length < 0 or length > maximum:
            raise XenoteerError("response_too_large", f"response exceeds {maximum} bytes")
    output = bytearray()
    async for chunk in response.aiter_bytes():
        output.extend(chunk)
        if len(output) > maximum:
            raise XenoteerError("response_too_large", f"response exceeds {maximum} bytes")
    return bytes(output)


def _json_size(value: object) -> int | None:
    if value is None:
        return 0
    try:
        return len(
            json.dumps(
                value,
                ensure_ascii=False,
                allow_nan=False,
                separators=(",", ":"),
                sort_keys=True,
            ).encode("utf-8")
        )
    except (TypeError, ValueError):
        return None


def _decode_json_response(response: Any, content: bytes) -> JsonObject:
    media_type = response.headers.get("content-type", "").split(";", 1)[0].strip().lower()
    try:
        decoded = json.loads(content.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
        raise XenoteerError("invalid_response", "server returned invalid UTF-8 JSON") from None
    if not 200 <= int(response.status_code) < 300:
        if media_type == "application/problem+json" and isinstance(decoded, dict):
            raise error_from_problem(int(response.status_code), decoded)
        raise XenoteerError(
            _status_code(int(response.status_code)),
            f"Xenoteer request failed with HTTP {response.status_code}",
            status=int(response.status_code),
        )
    if media_type != "application/json":
        raise XenoteerError("invalid_response", "successful response was not application/json")
    if not isinstance(decoded, dict):
        raise XenoteerError("invalid_response", "response must be a JSON object")
    return decoded


def _raise_http_error(response: Any, content: bytes) -> None:
    media_type = response.headers.get("content-type", "").split(";", 1)[0].strip().lower()
    decoded: object = None
    if content:
        try:
            decoded = json.loads(content.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
            decoded = None
    if media_type == "application/problem+json" and isinstance(decoded, dict):
        raise error_from_problem(int(response.status_code), decoded)
    status = int(response.status_code)
    raise XenoteerError(
        _status_code(status),
        f"Xenoteer request failed with HTTP {status}",
        status=status,
    )


def _status_code(status: int) -> str:
    if status == 401:
        return "authentication"
    if status == 403:
        return "permission"
    return "unexpected_http_status"


def _transport_error(error: Exception) -> XenoteerError:
    name = type(error).__name__.lower()
    if "timeout" in name:
        return XenoteerError("request_timeout", "Xenoteer request timed out")
    return XenoteerError("transport", "Xenoteer transport failed")


def _transport_failure(error: BaseException) -> BaseException:
    if isinstance(error, (XenoteerError, asyncio.CancelledError)):
        return error
    if isinstance(error, Exception):
        return _transport_error(error)
    return error


def _require_exact_header(headers: Any, name: str, expected: str) -> None:
    if headers.get(name) != expected:
        raise XenoteerError("invalid_response", f"artifact response {name} did not match")


def _is_async_callable(value: object) -> bool:
    return callable(value) and (
        inspect.iscoroutinefunction(value)
        or inspect.iscoroutinefunction(getattr(value, "__call__", None))
    )
