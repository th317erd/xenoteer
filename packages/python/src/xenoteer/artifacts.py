# SPDX-License-Identifier: Apache-2.0
"""Purpose-bound, digest-verified private artifact transfers."""

from __future__ import annotations

import datetime as dt
import asyncio
import hashlib
import inspect
import re
import uuid
from collections.abc import AsyncIterable, Awaitable, Callable, Mapping
from dataclasses import dataclass
from typing import Literal, TypeAlias
from urllib.parse import quote, urlencode

from .errors import XenoteerError
from .transport import AsyncTransport


MAX_ARTIFACT_BYTES = 32 * 1_024 * 1_024
MAX_CLIPBOARD_ARTIFACT_BYTES = 16 * 1_024 * 1_024
ArtifactPurpose: TypeAlias = Literal[
    "clipboard_input",
    "clipboard_output",
    "screenshot",
    "action_trace",
    "support_bundle",
]
ArtifactSink: TypeAlias = Callable[[bytes], None | Awaitable[None]]

_PURPOSE_LIMITS: dict[str, int] = {
    "clipboard_input": MAX_CLIPBOARD_ARTIFACT_BYTES,
    "clipboard_output": MAX_CLIPBOARD_ARTIFACT_BYTES,
    "screenshot": MAX_ARTIFACT_BYTES,
    "action_trace": MAX_ARTIFACT_BYTES,
    "support_bundle": MAX_ARTIFACT_BYTES,
}
_MEDIA_TOKEN = re.compile(r"[!#$&^_.+A-Za-z0-9-]+\Z")
_SHA256 = re.compile(r"[0-9a-f]{64}\Z")


def _uuid(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise XenoteerError("invalid_response", f"{label} is invalid")
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        raise XenoteerError("invalid_response", f"{label} is invalid") from None
    if parsed.int == 0:
        raise XenoteerError("invalid_response", f"{label} is invalid")
    return value


def _timestamp(value: object, label: str) -> tuple[str, dt.datetime]:
    if not isinstance(value, str):
        raise XenoteerError("invalid_response", f"{label} is invalid")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        raise XenoteerError("invalid_response", f"{label} is invalid") from None
    if parsed.tzinfo is None:
        raise XenoteerError("invalid_response", f"{label} is invalid")
    return value, parsed.astimezone(dt.timezone.utc)


def validate_content_type(value: object) -> str:
    """Validate Xenoteer's conservative media-type grammar."""

    if (
        not isinstance(value, str)
        or not 3 <= len(value.encode("utf-8")) <= 128
        or value.strip() != value
        or value.endswith(";")
        or any(ord(character) < 0x20 or ord(character) > 0x7E for character in value)
    ):
        raise XenoteerError("invalid_request", "artifact content type is invalid")
    essence = value.split(";", 1)[0]
    pieces = essence.split("/")
    if (
        len(pieces) != 2
        or _MEDIA_TOKEN.fullmatch(pieces[0]) is None
        or _MEDIA_TOKEN.fullmatch(pieces[1]) is None
    ):
        raise XenoteerError("invalid_request", "artifact content type is invalid")
    return value


@dataclass(frozen=True, slots=True)
class ArtifactRef:
    """Immutable metadata; its digest is deliberately redacted from diagnostics."""

    artifact_id: str
    purpose: ArtifactPurpose
    desktop_id: str
    desktop_generation: str
    content_type: str
    content_length: int
    sha256: str
    created_at: str
    expires_at: str

    @classmethod
    def from_wire(
        cls,
        value: object,
        *,
        desktop_id: str | None = None,
        desktop_generation: str | None = None,
        purpose: ArtifactPurpose | None = None,
    ) -> "ArtifactRef":
        if not isinstance(value, Mapping):
            raise XenoteerError("invalid_response", "artifact reference is invalid")
        actual_purpose = value.get("purpose")
        length = value.get("content_length")
        digest = value.get("sha256")
        if (
            actual_purpose not in _PURPOSE_LIMITS
            or isinstance(length, bool)
            or not isinstance(length, int)
            or not 1 <= length <= _PURPOSE_LIMITS[str(actual_purpose)]
            or not isinstance(digest, str)
            or _SHA256.fullmatch(digest) is None
        ):
            raise XenoteerError("invalid_response", "artifact reference is invalid")
        created_at, created = _timestamp(value.get("created_at"), "artifact creation time")
        expires_at, expires = _timestamp(value.get("expires_at"), "artifact expiry")
        if expires <= created:
            raise XenoteerError("invalid_response", "artifact retention is invalid")
        try:
            content_type = validate_content_type(value.get("content_type"))
        except XenoteerError:
            raise XenoteerError("invalid_response", "artifact content type is invalid") from None
        result = cls(
            artifact_id=_uuid(value.get("artifact_id"), "artifact ID"),
            purpose=actual_purpose,
            desktop_id=_uuid(value.get("desktop_id"), "artifact desktop ID"),
            desktop_generation=_uuid(
                value.get("desktop_generation"), "artifact desktop generation"
            ),
            content_type=content_type,
            content_length=length,
            sha256=digest,
            created_at=created_at,
            expires_at=expires_at,
        )
        result.require_scope(desktop_id, desktop_generation, purpose)
        return result

    def require_scope(
        self,
        desktop_id: str | None,
        desktop_generation: str | None,
        purpose: ArtifactPurpose | None = None,
    ) -> None:
        if desktop_id is not None and self.desktop_id != desktop_id:
            raise XenoteerError("stale_reference", "artifact belongs to another desktop")
        if (
            desktop_generation is not None
            and self.desktop_generation != desktop_generation
        ):
            raise XenoteerError(
                "stale_reference", "artifact belongs to another desktop generation"
            )
        if purpose is not None and self.purpose != purpose:
            raise XenoteerError("invalid_response", "artifact purpose is invalid")

    def wire(self) -> dict[str, object]:
        return {
            "artifact_id": self.artifact_id,
            "purpose": self.purpose,
            "desktop_id": self.desktop_id,
            "desktop_generation": self.desktop_generation,
            "content_type": self.content_type,
            "content_length": self.content_length,
            "sha256": self.sha256,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
        }

    def __repr__(self) -> str:
        return (
            "ArtifactRef("
            f"artifact_id={self.artifact_id!r}, purpose={self.purpose!r}, "
            f"desktop_id={self.desktop_id!r}, "
            f"desktop_generation={self.desktop_generation!r}, "
            f"content_type={self.content_type!r}, content_length={self.content_length}, "
            "sha256='<redacted>', "
            f"created_at={self.created_at!r}, expires_at={self.expires_at!r})"
        )


class Artifacts:
    """Bounded transfer operations fenced to a single desktop generation."""

    __slots__ = ("_desktop_generation", "_desktop_id", "_transport")

    def __init__(
        self,
        transport: AsyncTransport,
        desktop_id: str,
        desktop_generation: str,
    ) -> None:
        self._transport = transport
        self._desktop_id = desktop_id
        self._desktop_generation = desktop_generation

    async def upload_clipboard_input(
        self, content_type: str, body: bytes | bytearray | memoryview
    ) -> ArtifactRef:
        content_type = validate_content_type(content_type)
        if not isinstance(body, (bytes, bytearray, memoryview)):
            raise XenoteerError("invalid_request", "artifact body must be bytes")
        encoded = bytes(body)
        if not encoded:
            raise XenoteerError("invalid_request", "artifact body must not be empty")
        if len(encoded) > MAX_CLIPBOARD_ARTIFACT_BYTES:
            raise XenoteerError(
                "request_too_large",
                f"artifact exceeds {MAX_CLIPBOARD_ARTIFACT_BYTES} bytes",
            )
        upload = getattr(self._transport, "upload_artifact", None)
        if upload is None:
            raise XenoteerError(
                "transport", "configured transport does not support artifacts"
            )
        response = await upload(
            "/v1/artifacts?purpose=clipboard_input", content_type, encoded
        )
        artifact = ArtifactRef.from_wire(
            response,
            desktop_id=self._desktop_id,
            desktop_generation=self._desktop_generation,
            purpose="clipboard_input",
        )
        if (
            artifact.content_type != content_type
            or artifact.content_length != len(encoded)
            or artifact.sha256 != hashlib.sha256(encoded).hexdigest()
        ):
            raise XenoteerError(
                "invalid_response", "uploaded artifact metadata does not match its body"
            )
        return artifact

    async def upload_clipboard_input_stream(
        self,
        content_type: str,
        chunks: AsyncIterable[bytes],
        *,
        content_length: int,
        sha256: str,
    ) -> ArtifactRef:
        """Upload without collecting the caller's bounded async byte stream."""

        content_type = validate_content_type(content_type)
        if (
            isinstance(content_length, bool)
            or not isinstance(content_length, int)
            or not 1 <= content_length <= MAX_CLIPBOARD_ARTIFACT_BYTES
            or not isinstance(sha256, str)
            or _SHA256.fullmatch(sha256) is None
        ):
            raise XenoteerError(
                "invalid_request", "clipboard artifact stream metadata is invalid"
            )
        upload = getattr(self._transport, "upload_artifact_stream", None)
        if upload is None:
            raise XenoteerError(
                "transport", "configured transport does not support streamed artifacts"
            )
        response = await upload(
            "/v1/artifacts?purpose=clipboard_input",
            content_type,
            chunks,
            content_length=content_length,
            sha256=sha256,
        )
        artifact = ArtifactRef.from_wire(
            response,
            desktop_id=self._desktop_id,
            desktop_generation=self._desktop_generation,
            purpose="clipboard_input",
        )
        if (
            artifact.content_type != content_type
            or artifact.content_length != content_length
            or artifact.sha256 != sha256
        ):
            raise XenoteerError(
                "invalid_response", "uploaded artifact metadata does not match its stream"
            )
        return artifact

    async def download_to(self, artifact: ArtifactRef, sink: ArtifactSink) -> None:
        if not isinstance(artifact, ArtifactRef):
            raise XenoteerError("invalid_request", "artifact reference is invalid")
        artifact.require_scope(self._desktop_id, self._desktop_generation)
        if not callable(sink):
            raise XenoteerError("invalid_request", "artifact sink must be callable")
        download = getattr(self._transport, "download_artifact", None)
        if download is None:
            raise XenoteerError(
                "transport", "configured transport does not support artifacts"
            )

        async def checked_sink(chunk: bytes) -> None:
            try:
                outcome = sink(chunk)
                if inspect.isawaitable(outcome):
                    await outcome
            except asyncio.CancelledError:
                raise
            except Exception:
                raise XenoteerError(
                    "artifact_output", "artifact destination rejected output"
                ) from None

        await download(self._path(artifact), artifact, checked_sink)

    async def download_bytes(self, artifact: ArtifactRef) -> bytes:
        output = bytearray()

        def append(chunk: bytes) -> None:
            output.extend(chunk)

        await self.download_to(artifact, append)
        return bytes(output)

    async def delete(self, artifact: ArtifactRef) -> None:
        if not isinstance(artifact, ArtifactRef):
            raise XenoteerError("invalid_request", "artifact reference is invalid")
        artifact.require_scope(self._desktop_id, self._desktop_generation)
        delete = getattr(self._transport, "delete_artifact", None)
        if delete is None:
            raise XenoteerError(
                "transport", "configured transport does not support artifacts"
            )
        await delete(self._path(artifact))

    def _path(self, artifact: ArtifactRef) -> str:
        query = urlencode(
            {
                "desktop_id": self._desktop_id,
                "desktop_generation": self._desktop_generation,
            }
        )
        return f"/v1/artifacts/{quote(artifact.artifact_id, safe='')}?{query}"
