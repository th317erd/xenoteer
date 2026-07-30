# SPDX-License-Identifier: Apache-2.0
"""Immutable generation-bound desktop and domain APIs."""

from __future__ import annotations

import copy
import base64
import hashlib
import re
import uuid
from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, cast
from urllib.parse import quote, urlencode, urlsplit

from .artifacts import ArtifactRef, Artifacts
from .command import (
    CommandHandle,
    CommandSubmission,
    validate_client_command_envelope,
)
from .errors import XenoteerError
from .policy import ReferenceLifecycle
from .protocol_generated import JsonObject
from .state import GenerationRegistry
from .transport import AsyncTransport, request_with_deadline
from .wire import canonicalize_uint64_fields, validate_uint64_fields


_OPAQUE = re.compile(r"[A-Za-z0-9_-]{16,512}\Z")


def _new_id() -> str:
    return str(uuid.uuid4())


def _validate_uuid(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise XenoteerError("invalid_response", f"{label} must be a UUID")
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        raise XenoteerError("invalid_response", f"{label} must be a UUID") from None
    if parsed.int == 0:
        raise XenoteerError("invalid_response", f"{label} must be non-nil")
    return value


def _bounded_int(value: object, minimum: int, maximum: int, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise XenoteerError(
            "invalid_request", f"{label} must be an integer in {minimum}..{maximum}"
        )
    return value


def _mapping(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, Mapping):
        raise XenoteerError("invalid_request", f"{label} must be an object")
    if not all(isinstance(key, str) for key in value):
        raise XenoteerError("invalid_request", f"{label} keys must be strings")
    return copy.deepcopy(dict(value))


def _validate_response(response: object, desktop: "Desktop") -> JsonObject:
    if not isinstance(response, dict):
        raise XenoteerError("invalid_response", "server response must be an object")
    if "desktop_id" in response and response["desktop_id"] != desktop.id:
        raise XenoteerError("invalid_response", "response belongs to another desktop")
    if (
        "desktop_generation" in response
        and response["desktop_generation"] != desktop.generation
    ):
        if isinstance(response["desktop_generation"], str):
            desktop.registry.observe(response["desktop_generation"])
        raise XenoteerError(
            "invalid_response", "response belongs to another desktop generation"
        )
    try:
        validate_uint64_fields(response)
    except (TypeError, ValueError):
        raise XenoteerError(
            "invalid_response", "response contains an invalid uint64 wire value"
        ) from None
    return cast(JsonObject, copy.deepcopy(response))


class Desktop:
    """Cheap immutable handle fenced to exactly one desktop lifetime."""

    __slots__ = (
        "_accessibility",
        "_applications",
        "_artifacts",
        "_capture",
        "_clipboard",
        "_generation",
        "_id",
        "_owned_leases",
        "_protocol",
        "_registry",
        "_transport",
        "_viewer",
        "_windows",
    )

    def __init__(
        self,
        transport: AsyncTransport,
        desktop_id: str,
        generation: str,
        protocol: Mapping[str, int],
        registry: GenerationRegistry | None = None,
        owned_leases: list["ControlLease"] | None = None,
    ) -> None:
        self._transport = transport
        self._id = _validate_uuid(desktop_id, "desktop ID")
        self._generation = _validate_uuid(generation, "desktop generation")
        self._protocol = dict(protocol)
        self._registry = (
            GenerationRegistry(self._generation) if registry is None else registry
        )
        self._registry.observe(self._generation)
        self._owned_leases = owned_leases
        self._windows = Windows(self)
        self._accessibility = Accessibility(self)
        self._clipboard = Clipboard(self)
        self._capture = Capture(self)
        self._viewer = Viewer(self)
        self._applications = Applications(self)
        self._artifacts = Artifacts(transport, self.id, self.generation)

    @property
    def id(self) -> str:
        return self._id

    @property
    def generation(self) -> str:
        return self._generation

    @property
    def protocol(self) -> dict[str, int]:
        return dict(self._protocol)

    @property
    def registry(self) -> GenerationRegistry:
        return self._registry

    @property
    def windows(self) -> "Windows":
        return self._windows

    @property
    def accessibility(self) -> "Accessibility":
        return self._accessibility

    @property
    def clipboard(self) -> "Clipboard":
        return self._clipboard

    @property
    def capture(self) -> "Capture":
        return self._capture

    @property
    def viewer(self) -> "Viewer":
        return self._viewer

    @property
    def applications(self) -> "Applications":
        return self._applications

    @property
    def artifacts(self) -> Artifacts:
        return self._artifacts

    def submit(
        self,
        command: Mapping[str, Any],
        *,
        command_id: str | None = None,
        lease_id: str | None = None,
        deadline: str | None = None,
        trace_policy: str | None = None,
        _lifecycle: ReferenceLifecycle | None = None,
    ) -> CommandSubmission:
        """Prepare an exact submission before any network I/O occurs."""

        body_command = _mapping(command, "command")
        if not isinstance(body_command.get("type"), str) or not body_command["type"]:
            raise XenoteerError("invalid_request", "command type is required")
        command_id = _new_id() if command_id is None else _validate_uuid(
            command_id, "command ID"
        )
        if lease_id is not None:
            lease_id = _validate_uuid(lease_id, "lease ID")
        if deadline is not None and (
            not isinstance(deadline, str)
            or "T" not in deadline
            or not deadline.endswith(("Z", "+00:00"))
        ):
            raise XenoteerError("invalid_request", "deadline must be an RFC 3339 timestamp")
        if trace_policy is not None and trace_policy not in {
            "none",
            "normal",
            "detailed",
        }:
            raise XenoteerError("invalid_request", "trace policy is invalid")
        try:
            normalized_command = canonicalize_uint64_fields(body_command)
        except (TypeError, ValueError):
            raise XenoteerError(
                "invalid_request", "command contains an invalid uint64 value"
            ) from None
        envelope = {
            "protocol_version": self.protocol,
            "request_id": _new_id(),
            "command_id": command_id,
            "desktop_id": self.id,
            "desktop_generation": self.generation,
            "lease_id": lease_id,
            "deadline": deadline,
            "trace_policy": trace_policy,
            "command": normalized_command,
        }
        envelope = validate_client_command_envelope(envelope)
        return CommandSubmission(
            self._transport,
            self.id,
            self.generation,
            envelope,
            registry=self.registry,
            lifecycle=_lifecycle,
            artifacts=self.artifacts,
        )

    async def command(self, command_id: str) -> CommandHandle:
        """Attach to an existing ledger entry without submitting new work."""

        command_id = _validate_uuid(command_id, "command ID")
        path = (
            f"/v1/desktops/{quote(self.id, safe='')}"
            f"/commands/{quote(command_id, safe='')}"
        )
        response = await self._transport.request("GET", path)
        return CommandHandle(
            self._transport,
            self.id,
            self.generation,
            response,
            registry=self.registry,
        )

    async def acquire_control(self, ttl: float | None = None) -> "ControlLease":
        """Acquire exclusive physical control without hidden auto-renewal."""

        from .lease import ControlLease

        ttl_ms: int | None = None
        if ttl is not None:
            if (
                isinstance(ttl, bool)
                or not isinstance(ttl, (int, float))
                or not 0 < ttl <= 3600
            ):
                raise XenoteerError("invalid_request", "lease TTL must be in (0, 3600]s")
            ttl_ms = max(1, int(ttl * 1000))
        body: dict[str, Any] = {
            "protocol_version": self.protocol,
            "request_id": _new_id(),
            "desktop_id": self.id,
            "desktop_generation": self.generation,
        }
        if ttl_ms is not None:
            body["ttl_ms"] = ttl_ms
        response = await self._transport.request(
            "POST", f"/v1/desktops/{quote(self.id, safe='')}/lease", body
        )
        lease = ControlLease(self, self._transport, response, ttl_ms)
        if self._owned_leases is not None:
            self._owned_leases.append(lease)
        return lease

    async def control_state(self) -> JsonObject:
        """Read the caller-redacted lease state without acquiring control."""

        response = await self._transport.request(
            "GET", f"/v1/desktops/{quote(self.id, safe='')}/lease"
        )
        return _validate_response(response, self)

    def control(self, ttl: float | None = None) -> "ControlContext":
        """Return an async context that guarantees an awaited release."""

        from .lease import ControlContext

        return ControlContext(self, ttl)

    def __repr__(self) -> str:
        return f"Desktop(id={self.id!r}, generation={self.generation!r})"


class Windows:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    async def list(
        self,
        *,
        limit: int = 100,
        order: str = "creation_ascending",
        cursor: str | None = None,
    ) -> JsonObject:
        _bounded_int(limit, 1, 1000, "window page limit")
        params = {
            "desktop_generation": self._desktop.generation,
            "limit": str(limit),
            "order": order,
        }
        if cursor is not None:
            if _OPAQUE.fullmatch(cursor) is None:
                raise XenoteerError("invalid_request", "window cursor is invalid")
            params["cursor"] = cursor
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}/windows?"
            f"{urlencode(params)}"
        )
        return _validate_response(
            await self._desktop._transport.request("GET", path), self._desktop
        )

    async def query(self, selector: Mapping[str, Any], **options: Any) -> JsonObject:
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "selector": _mapping(selector, "window selector"),
            "order": options.pop("order", "creation_ascending"),
            "limit": options.pop("limit", 100),
            "cursor": options.pop("cursor", None),
            **options,
        }
        _bounded_int(body["limit"], 1, 1000, "window page limit")
        path = f"/v1/desktops/{quote(self._desktop.id, safe='')}/windows/query"
        response = await self._desktop._transport.request("POST", path, body)
        return _validate_response(response, self._desktop)

    async def wait(self, request: Mapping[str, Any]) -> JsonObject:
        body = _mapping(request, "window wait request")
        body["desktop_id"] = self._desktop.id
        body["desktop_generation"] = self._desktop.generation
        _bounded_int(body.get("timeout_ms"), 1, 300_000, "window wait timeout")
        try:
            body = canonicalize_uint64_fields(body)  # type: ignore[assignment]
        except (TypeError, ValueError):
            raise XenoteerError("invalid_request", "window wait request is invalid") from None
        path = f"/v1/desktops/{quote(self._desktop.id, safe='')}/windows/wait"
        return _validate_response(
            await request_with_deadline(
                self._desktop._transport,
                "POST",
                path,
                body,
                timeout=float(body["timeout_ms"]) / 1_000 + 5,
            ),
            self._desktop,
        )

    async def one(
        self,
        selector: Mapping[str, Any],
        *,
        order: str = "creation_ascending",
    ) -> "Window":
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "selector": _mapping(selector, "window selector"),
            "order": order,
            "match_policy": "exactly_one",
        }
        path = f"/v1/desktops/{quote(self._desktop.id, safe='')}/windows/resolve"
        response = _validate_response(
            await self._desktop._transport.request("POST", path, body), self._desktop
        )
        return Window.from_entry(self._desktop, response.get("window"))

    def handle(
        self, reference: Mapping[str, Any], *, reference_token: str | None = None
    ) -> "Window":
        return Window(self._desktop, reference, reference_token=reference_token)


class Window:
    """Exact observed window-birth handle; it never silently relocates."""

    __slots__ = ("_desktop", "_lifecycle", "_reference", "_reference_token")

    def __init__(
        self,
        desktop: Desktop,
        reference: Mapping[str, Any],
        *,
        reference_token: str | None = None,
    ) -> None:
        ref = _mapping(reference, "window reference")
        if (
            ref.get("desktop_id") != desktop.id
            or ref.get("desktop_generation") != desktop.generation
        ):
            raise XenoteerError("invalid_request", "window belongs to another desktop generation")
        try:
            normalized_ref = canonicalize_uint64_fields(ref)
        except (TypeError, ValueError):
            raise XenoteerError("invalid_request", "window reference is invalid") from None
        if not isinstance(normalized_ref, dict):
            raise XenoteerError("invalid_request", "window reference is invalid")
        if reference_token is not None and _OPAQUE.fullmatch(reference_token) is None:
            raise XenoteerError("invalid_request", "window reference token is invalid")
        self._desktop = desktop
        self._reference = cast(JsonObject, normalized_ref)
        self._reference_token = reference_token
        self._lifecycle = ReferenceLifecycle(self._reference, desktop.registry)

    @classmethod
    def from_entry(cls, desktop: Desktop, entry: object) -> "Window":
        if not isinstance(entry, Mapping):
            raise XenoteerError("invalid_response", "window entry is invalid")
        snapshot = entry.get("snapshot")
        reference = snapshot.get("ref") if isinstance(snapshot, Mapping) else None
        token = entry.get("reference_token")
        if not isinstance(reference, Mapping) or not isinstance(token, str):
            raise XenoteerError("invalid_response", "window entry is invalid")
        return cls(desktop, reference, reference_token=token)

    @property
    def ref(self) -> JsonObject:
        self._lifecycle.require_current()
        return copy.deepcopy(self._reference)

    @property
    def identity(self) -> JsonObject:
        """Return immutable identity evidence even after the handle becomes stale."""

        return copy.deepcopy(self._reference)

    @property
    def stale(self) -> bool:
        return self._lifecycle.stale

    def invalidate(self, reason: str) -> None:
        """Mark a destroy/restart observation without retargeting this handle."""

        self._lifecycle.invalidate(reason)

    async def relocate(
        self,
        selector: Mapping[str, Any],
        *,
        order: str = "creation_ascending",
    ) -> "Window":
        """Resolve a fresh identity without mutating this handle."""

        return await self._desktop.windows.one(selector, order=order)

    def _submit(self, command: Mapping[str, Any]) -> CommandSubmission:
        return self._desktop.submit(command, _lifecycle=self._lifecycle)

    async def snapshot(self) -> JsonObject:
        self._lifecycle.require_current()
        if self._reference_token is None:
            raise XenoteerError(
                "invalid_request", "snapshot requires a server-issued reference token"
            )
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}/windows/"
            f"{quote(self._reference_token, safe='')}?"
            f"{urlencode({'desktop_generation': self._desktop.generation})}"
        )
        try:
            response = await self._desktop._transport.request("GET", path)
        except XenoteerError as error:
            if error.code == "stale_reference" or error.problem_code == "stale_reference":
                self._lifecycle.invalidate("server_stale_reference")
            raise
        return _validate_response(response, self._desktop)

    def activate(
        self, *, switch_workspace: bool = False, allow_focus_fallback: bool = False
    ) -> CommandSubmission:
        return self._submit(
            {
                "type": "window_activate",
                "window": self.ref,
                "switch_workspace": switch_workspace,
                "fallback": (
                    "allow_set_input_focus" if allow_focus_fallback else "ewmh_only"
                ),
            }
        )

    def close(self, *, wait_for: str = "unmapped_or_destroyed") -> CommandSubmission:
        return self._submit(
            {"type": "window_close", "window": self.ref, "wait_for": wait_for}
        )

    def move_resize(
        self,
        *,
        x: int | None = None,
        y: int | None = None,
        width: int | None = None,
        height: int | None = None,
        relative_to: str = "frame",
        bounds_policy: str = "require_inside_root",
    ) -> CommandSubmission:
        geometry = {
            key: value
            for key, value in {"x": x, "y": y, "width": width, "height": height}.items()
            if value is not None
        }
        if not geometry:
            raise XenoteerError("invalid_request", "window geometry cannot be empty")
        for key in ("x", "y"):
            if key in geometry:
                _bounded_int(geometry[key], -(1 << 31), (1 << 31) - 1, key)
        for key in ("width", "height"):
            if key in geometry:
                _bounded_int(geometry[key], 1, 65_535, key)
        return self._submit(
            {
                "type": "window_move_resize",
                "window": self.ref,
                "relative_to": relative_to,
                "geometry": geometry,
                "bounds_policy": bounds_policy,
            }
        )

    def set_state(self, state: str, desired: bool) -> CommandSubmission:
        return self._submit(
            {
                "type": "window_set_state",
                "window": self.ref,
                "state": state,
                "desired": bool(desired),
            }
        )

    def minimize(self, desired: bool = True) -> CommandSubmission:
        return self._submit(
            {
                "type": "window_minimize",
                "window": self.ref,
                "desired": bool(desired),
            }
        )

    def move_to_workspace(self, workspace: int) -> CommandSubmission:
        _bounded_int(workspace, 0, (1 << 32) - 2, "workspace")
        return self._submit(
            {
                "type": "window_move_to_workspace",
                "window": self.ref,
                "workspace": workspace,
            }
        )

    def stack(
        self,
        mode: str,
        *,
        sibling: "Window | Mapping[str, Any] | None" = None,
    ) -> CommandSubmission:
        if mode not in {"raise", "lower", "above", "below"}:
            raise XenoteerError("invalid_request", "window stack mode is invalid")
        sibling_ref = (
            None
            if sibling is None
            else sibling.ref
            if isinstance(sibling, Window)
            else _mapping(sibling, "sibling window reference")
        )
        if (mode in {"above", "below"}) != (sibling_ref is not None):
            raise XenoteerError(
                "invalid_request", "above/below require exactly one sibling"
            )
        return self._submit(
            {
                "type": "window_stack",
                "window": self.ref,
                "mode": mode,
                "sibling": sibling_ref,
            }
        )


class Accessibility:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    async def query(self, selector: Mapping[str, Any], **options: Any) -> JsonObject:
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "selector": _mapping(selector, "element selector"),
            **options,
        }
        body = canonicalize_uint64_fields(body)  # type: ignore[assignment]
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}"
            "/accessibility/elements/query"
        )
        return _validate_response(
            await self._desktop._transport.request("POST", path, body),
            self._desktop,
        )

    async def list(self, request: Mapping[str, Any]) -> JsonObject:
        body = _mapping(request, "element list request")
        body["desktop_id"] = self._desktop.id
        body["desktop_generation"] = self._desktop.generation
        try:
            body = canonicalize_uint64_fields(body)  # type: ignore[assignment]
        except (TypeError, ValueError):
            raise XenoteerError("invalid_request", "element list request is invalid") from None
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}"
            "/accessibility/elements/list"
        )
        return _validate_response(
            await self._desktop._transport.request("POST", path, body), self._desktop
        )

    async def one(self, selector: Mapping[str, Any], **options: Any) -> "Element":
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "selector": _mapping(selector, "element selector"),
            **options,
        }
        body = canonicalize_uint64_fields(body)  # type: ignore[assignment]
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}"
            "/accessibility/elements/resolve"
        )
        response = _validate_response(
            await self._desktop._transport.request("POST", path, body), self._desktop
        )
        entry = response.get("element")
        if not isinstance(entry, Mapping):
            raise XenoteerError("invalid_response", "element resolve result is invalid")
        snapshot = entry.get("snapshot")
        if not isinstance(snapshot, Mapping):
            raise XenoteerError("invalid_response", "element resolve result is invalid")
        reference = snapshot.get("ref")
        if not isinstance(reference, Mapping):
            raise XenoteerError("invalid_response", "element resolve result is invalid")
        return Element(self._desktop, reference)

    async def wait(self, request: Mapping[str, Any]) -> JsonObject:
        body = _mapping(request, "element wait request")
        body["desktop_id"] = self._desktop.id
        body["desktop_generation"] = self._desktop.generation
        timeout_ms = _bounded_int(
            body.get("timeout_ms"), 1, 120_000, "element wait timeout"
        )
        body = canonicalize_uint64_fields(body)  # type: ignore[assignment]
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}"
            "/accessibility/elements/wait"
        )
        return _validate_response(
            await request_with_deadline(
                self._desktop._transport,
                "POST",
                path,
                body,
                timeout=float(timeout_ms) / 1_000 + 5,
            ),
            self._desktop,
        )

    def handle(self, reference: Mapping[str, Any]) -> "Element":
        return Element(self._desktop, reference)


class Element:
    """Exact AT-SPI object identity fenced to one bus/application generation."""

    __slots__ = ("_desktop", "_lifecycle", "_reference")

    def __init__(self, desktop: Desktop, reference: Mapping[str, Any]) -> None:
        ref = _mapping(reference, "element reference")
        if (
            ref.get("desktop_id") != desktop.id
            or ref.get("desktop_generation") != desktop.generation
        ):
            raise XenoteerError("invalid_request", "element belongs to another generation")
        try:
            normalized_ref = canonicalize_uint64_fields(ref)
        except (TypeError, ValueError):
            raise XenoteerError("invalid_request", "element reference is invalid") from None
        self._desktop = desktop
        if not isinstance(normalized_ref, dict):
            raise XenoteerError("invalid_request", "element reference is invalid")
        self._reference = cast(JsonObject, normalized_ref)
        self._lifecycle = ReferenceLifecycle(self._reference, desktop.registry)

    @property
    def ref(self) -> JsonObject:
        self._lifecycle.require_current()
        return copy.deepcopy(self._reference)

    @property
    def identity(self) -> JsonObject:
        return copy.deepcopy(self._reference)

    @property
    def stale(self) -> bool:
        return self._lifecycle.stale

    def invalidate(self, reason: str) -> None:
        """Mark application/bus discontinuity without retargeting this handle."""

        self._lifecycle.invalidate(reason)

    async def relocate(
        self, selector: Mapping[str, Any], **options: Any
    ) -> "Element":
        """Resolve a fresh identity without mutating this handle."""

        return await self._desktop.accessibility.one(selector, **options)

    def _submit(self, command: Mapping[str, Any]) -> CommandSubmission:
        return self._desktop.submit(command, _lifecycle=self._lifecycle)

    async def snapshot(self, *, expansion: Mapping[str, Any] | None = None) -> JsonObject:
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "element": self.ref,
            "expansion": {} if expansion is None else _mapping(expansion, "expansion"),
        }
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}"
            "/accessibility/elements/snapshot"
        )
        try:
            response = await self._desktop._transport.request("POST", path, body)
        except XenoteerError as error:
            if error.code == "stale_reference" or error.problem_code == "stale_reference":
                self._lifecycle.invalidate("server_stale_reference")
            raise
        try:
            validate_uint64_fields(response)
        except (TypeError, ValueError):
            raise XenoteerError("invalid_response", "element snapshot is invalid") from None
        return copy.deepcopy(response)

    def invoke(
        self,
        action: str | int | None = None,
        *,
        allow_disabled: bool = False,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        if action is None:
            action_wire: dict[str, Any] = {"type": "default"}
        elif isinstance(action, str) and action:
            action_wire = {"type": "name", "name": action}
        elif isinstance(action, int) and not isinstance(action, bool):
            action_wire = {"type": "index", "index": action}
        else:
            raise XenoteerError("invalid_request", "element action is invalid")
        return self._submit(
            {
                "type": "element_invoke",
                "element": self.ref,
                "action": action_wire,
                "allow_disabled": allow_disabled,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def focus(
        self,
        *,
        require_window_focus_correlation: bool = True,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        return self._submit(
            {
                "type": "element_focus",
                "element": self.ref,
                "require_window_focus_correlation": require_window_focus_correlation,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def set_value(
        self,
        value: float,
        *,
        tolerance: float | None = None,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise XenoteerError("invalid_request", "element value must be numeric")
        return self._submit(
            {
                "type": "element_set_value",
                "element": self.ref,
                "value": value,
                "tolerance": tolerance,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def selection(
        self,
        operation: Mapping[str, Any],
        *,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        return self._submit(
            {
                "type": "element_selection",
                "element": self.ref,
                "operation": _mapping(operation, "selection operation"),
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def set_text(
        self,
        text: str,
        *,
        selection: str = "collapse_after",
        verify_length_only: bool = True,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        if not isinstance(text, str) or "\0" in text or len(text.encode()) > 256 * 1024:
            raise XenoteerError("invalid_request", "semantic text is invalid")
        return self._submit(
            {
                "type": "element_set_text",
                "element": self.ref,
                "text": text,
                "selection": selection,
                "verify_length_only": verify_length_only,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def insert_text(
        self,
        offset: int,
        text: str,
        *,
        selection: str = "collapse_after",
        verify_length_only: bool = True,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        _bounded_int(offset, 0, (1 << 31) - 2, "text offset")
        if not isinstance(text, str) or "\0" in text or len(text.encode()) > 256 * 1024:
            raise XenoteerError("invalid_request", "semantic text is invalid")
        return self._submit(
            {
                "type": "element_insert_text",
                "element": self.ref,
                "offset": offset,
                "text": text,
                "selection": selection,
                "verify_length_only": verify_length_only,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def scroll(
        self,
        target: Mapping[str, Any] | None = None,
        *,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        return self._submit(
            {
                "type": "element_scroll",
                "element": self.ref,
                "target": (
                    {"type": "alignment", "alignment": "anywhere"}
                    if target is None
                    else _mapping(target, "scroll target")
                ),
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            }
        )

    def physical_click(
        self,
        control: "ControlLease",
        *,
        window: Window | Mapping[str, Any] | None = None,
        button: str = "left",
        count: int = 1,
        move_duration: float | None = None,
        postcondition: Mapping[str, Any] | None = None,
    ) -> CommandSubmission:
        _bounded_int(count, 1, 5, "click count")
        window_ref = (
            None
            if window is None
            else window.ref
            if isinstance(window, Window)
            else _mapping(window, "window reference")
        )
        duration_ms = None
        if move_duration is not None:
            if (
                isinstance(move_duration, bool)
                or not isinstance(move_duration, (int, float))
                or not 0 < move_duration <= 10
            ):
                raise XenoteerError(
                    "invalid_request", "physical click duration must be in (0, 10]s"
                )
            duration_ms = int(move_duration * 1000)
        return control.submit(
            {
                "type": "element_physical_click",
                "element": self.ref,
                "window": window_ref,
                "minimum_correlation": "strong",
                "point_policy": {"type": "center"},
                "scroll_policy": "if_needed",
                "activation_policy": "if_needed",
                "occlusion_policy": "best_effort_reject",
                "button": button,
                "count": count,
                "interval_ms": 100,
                "move_duration_ms": duration_ms,
                "curve": "smooth",
                "settle_timeout_ms": 3000,
                "postcondition": (
                    None
                    if postcondition is None
                    else _mapping(postcondition, "postcondition")
                ),
            },
            _lifecycle=self._lifecycle,
        )


class Clipboard:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    async def read(
        self,
        selection: str = "clipboard",
        *,
        preferred_targets: list[str] | None = None,
        allow_binary_fallback: bool = False,
    ) -> JsonObject:
        body = {
            "selection": selection,
            "preferred_targets": [] if preferred_targets is None else list(preferred_targets),
            "allow_binary_fallback": allow_binary_fallback,
        }
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}/clipboard/read?"
            f"{urlencode({'desktop_generation': self._desktop.generation})}"
        )
        response = await self._desktop._transport.request("POST", path, body)
        try:
            validate_uint64_fields(response)
        except (TypeError, ValueError):
            raise XenoteerError("invalid_response", "clipboard result is invalid") from None
        return copy.deepcopy(response)

    async def read_bytes(
        self,
        selection: str = "clipboard",
        *,
        preferred_targets: list[str] | None = None,
        allow_binary_fallback: bool = False,
    ) -> bytes:
        """Read and verify inline or private-artifact clipboard content."""

        response = await self.read(
            selection,
            preferred_targets=preferred_targets,
            allow_binary_fallback=allow_binary_fallback,
        )
        content = response.get("content")
        evidence = response.get("evidence")
        if not isinstance(content, Mapping) or not isinstance(evidence, Mapping):
            raise XenoteerError("invalid_response", "clipboard result is invalid")
        delivery = content.get("delivery")
        text = content.get("text")
        data = content.get("data")
        if delivery == "inline_text" and isinstance(text, str):
            body = text.encode("utf-8")
        elif delivery == "inline_binary" and isinstance(data, str):
            try:
                body = base64.b64decode(data, validate=True)
            except (ValueError, TypeError):
                raise XenoteerError(
                    "invalid_response", "clipboard binary body is invalid"
                ) from None
        elif delivery == "artifact":
            artifact = ArtifactRef.from_wire(
                content.get("artifact"),
                desktop_id=self._desktop.id,
                desktop_generation=self._desktop.generation,
                purpose="clipboard_output",
            )
            body = await self._desktop.artifacts.download_bytes(artifact)
        else:
            raise XenoteerError("invalid_response", "clipboard delivery is invalid")
        expected_length = evidence.get("content_length")
        expected_digest = evidence.get("sha256")
        if (
            isinstance(expected_length, bool)
            or not isinstance(expected_length, int)
            or expected_length != len(body)
            or not isinstance(expected_digest, str)
            or hashlib.sha256(body).hexdigest() != expected_digest
        ):
            raise XenoteerError(
                "invalid_response", "clipboard transfer evidence did not match"
            )
        return body


class Capture:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    async def screenshot(
        self,
        *,
        target: Mapping[str, Any] | None = None,
        region: Mapping[str, Any] | None = None,
        format: str = "png",
        include_cursor: bool = True,
        scale: Mapping[str, Any] | None = None,
        max_bytes: int | None = None,
    ) -> JsonObject:
        if max_bytes is not None:
            _bounded_int(max_bytes, 1, 32 * 1_048_576, "screenshot max bytes")
        body = {
            "target": {"kind": "root"} if target is None else _mapping(target, "target"),
            "region": None if region is None else _mapping(region, "region"),
            "format": format,
            "include_cursor": include_cursor,
            "scale": None if scale is None else _mapping(scale, "scale"),
            "max_bytes": max_bytes,
        }
        path = (
            f"/v1/desktops/{quote(self._desktop.id, safe='')}/screenshots?"
            f"{urlencode({'desktop_generation': self._desktop.generation})}"
        )
        return _validate_response(
            await self._desktop._transport.request("POST", path, body), self._desktop
        )

    async def screenshot_bytes(self, **options: Any) -> bytes:
        """Capture and verify bytes delivered through the private artifact API."""

        response = await self.screenshot(**options)
        delivery = response.get("delivery")
        if not isinstance(delivery, Mapping) or delivery.get("delivery") != "artifact":
            raise XenoteerError(
                "invalid_response", "screenshot did not contain a downloadable artifact"
            )
        artifact = ArtifactRef.from_wire(
            delivery.get("artifact"),
            desktop_id=self._desktop.id,
            desktop_generation=self._desktop.generation,
            purpose="screenshot",
        )
        digest = response.get("sha256")
        if not isinstance(digest, str) or digest != artifact.sha256:
            raise XenoteerError(
                "invalid_response", "screenshot digest did not match its artifact"
            )
        return await self._desktop.artifacts.download_bytes(artifact)


class ViewerTicket:
    """One-use browser ticket whose repr never exposes bearer material."""

    __slots__ = ("__ticket", "_metadata")

    def __init__(self, response: Mapping[str, Any]) -> None:
        ticket = response.get("ticket")
        if not isinstance(ticket, str) or not 43 <= len(ticket) <= 128:
            raise XenoteerError("invalid_response", "viewer ticket is invalid")
        self.__ticket = ticket
        self._metadata = {key: copy.deepcopy(value) for key, value in response.items() if key != "ticket"}

    def expose_ticket(self) -> str:
        """Return bearer material only at the viewer bootstrap boundary."""

        return self.__ticket

    @property
    def metadata(self) -> JsonObject:
        return cast(JsonObject, copy.deepcopy(self._metadata))

    def __repr__(self) -> str:
        return f"ViewerTicket(ticket=<redacted>, metadata={self._metadata!r})"


class Viewer:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    async def ticket(self, origin: str) -> ViewerTicket:
        parsed = urlsplit(origin)
        if (
            parsed.scheme not in {"http", "https"}
            or not parsed.netloc
            or parsed.username is not None
            or parsed.password is not None
            or parsed.path
            or parsed.query
            or parsed.fragment
        ):
            raise XenoteerError("invalid_request", "viewer origin is invalid")
        body = {
            "desktop_id": self._desktop.id,
            "desktop_generation": self._desktop.generation,
            "mode": "view_only",
        }
        path = f"/v1/desktops/{quote(self._desktop.id, safe='')}/viewer-tickets"
        response = _validate_response(
            await self._desktop._transport.request(
                "POST", path, body, headers={"origin": origin}
            ),
            self._desktop,
        )
        return ViewerTicket(response)


class Applications:
    __slots__ = ("_desktop",)

    def __init__(self, desktop: Desktop) -> None:
        self._desktop = desktop

    def launch(
        self, application: str, arguments: list[str] | None = None
    ) -> CommandSubmission:
        return self._desktop.submit(
            {
                "type": "application_launch",
                "application": application,
                "arguments": [] if arguments is None else list(arguments),
            }
        )

    def status(self, process: Mapping[str, Any]) -> CommandSubmission:
        return self._desktop.submit(
            {"type": "process_status", "process": _mapping(process, "process reference")}
        )

    def terminate(
        self, process: Mapping[str, Any], *, grace: float | None = None
    ) -> CommandSubmission:
        grace_ms = None
        if grace is not None:
            if (
                isinstance(grace, bool)
                or not isinstance(grace, (int, float))
                or not 0 <= grace <= 300
            ):
                raise XenoteerError("invalid_request", "process grace is outside 0..300s")
            grace_ms = int(grace * 1000)
        return self._desktop.submit(
            {
                "type": "process_terminate",
                "process": _mapping(process, "process reference"),
                "grace_ms": grace_ms,
            }
        )


if TYPE_CHECKING:
    from .lease import ControlContext, ControlLease
