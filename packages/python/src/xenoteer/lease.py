# SPDX-License-Identifier: Apache-2.0
"""Explicit async control-lease lifecycle and physical input domains."""

from __future__ import annotations

import asyncio
import copy
from collections.abc import Mapping
from types import TracebackType
from typing import TYPE_CHECKING, Any, cast
from urllib.parse import quote

from .artifacts import ArtifactRef
from .command import CommandSubmission
from .errors import XenoteerError
from .transport import AsyncTransport


def _request_id() -> str:
    from .desktop import _new_id

    return _new_id()


def _validate_state(
    state: object, desktop: "Desktop", *, owned: bool
) -> dict[str, Any]:
    if not isinstance(state, dict):
        raise XenoteerError("invalid_response", "lease state must be an object")
    expected = "held_by_caller" if owned else "vacant"
    if (
        state.get("desktop_id") != desktop.id
        or state.get("desktop_generation") != desktop.generation
        or state.get("state") != expected
        or (owned and not isinstance(state.get("lease_id"), str))
        or (owned and not isinstance(state.get("expires_at"), str))
        or (not owned and state.get("lease_id") is not None)
    ):
        raise XenoteerError("invalid_response", "server returned an invalid lease state")
    return copy.deepcopy(state)


def _milliseconds(
    value: float | int | None, maximum: int, label: str
) -> int | None:
    if value is None:
        return None
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or value < 0
        or value * 1000 > maximum
    ):
        raise XenoteerError(
            "invalid_request", f"{label} must be between 0 and {maximum / 1000:g}s"
        )
    return int(value * 1000)


def _point(x: int, y: int) -> dict[str, int]:
    for label, value in (("x", x), ("y", y)):
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or not -(1 << 31) <= value <= (1 << 31) - 1
        ):
            raise XenoteerError(
                "invalid_request", f"pointer {label} must be a signed 32-bit integer"
            )
    return {"x": x, "y": y}


def _key(value: str | Mapping[str, Any]) -> dict[str, Any]:
    if isinstance(value, str) and value:
        if len(value) == 1 and not value.isspace():
            return {"kind": "scalar", "value": value}
        return {"kind": "named", "name": value}
    if isinstance(value, Mapping):
        return copy.deepcopy(dict(value))
    raise XenoteerError("invalid_request", "keyboard key is invalid")


class Mouse:
    """Physical pointer API available only through an owned lease."""

    __slots__ = ("_lease",)

    def __init__(self, lease: "ControlLease") -> None:
        self._lease = lease

    def move(
        self,
        x: int,
        y: int,
        *,
        duration: float | None = None,
        curve: str = "smooth",
    ) -> CommandSubmission:
        """Move through interpolated samples; smooth is the safe default."""

        duration_ms = _milliseconds(duration, 10_000, "pointer duration")
        if curve not in {"instant", "linear", "smooth"}:
            raise XenoteerError("invalid_request", "pointer curve is invalid")
        if curve == "instant" and duration_ms not in {None, 0}:
            raise XenoteerError("invalid_request", "instant motion cannot have a duration")
        return self._lease.submit(
            {
                "type": "pointer_move",
                "target": _point(x, y),
                "duration_ms": duration_ms,
                "curve": curve,
            }
        )

    def click(
        self,
        x: int | None = None,
        y: int | None = None,
        *,
        button: str = "left",
        count: int = 1,
        duration: float | None = None,
    ) -> CommandSubmission:
        if (x is None) != (y is None):
            raise XenoteerError("invalid_request", "click requires both x and y")
        if isinstance(count, bool) or not isinstance(count, int) or not 1 <= count <= 5:
            raise XenoteerError("invalid_request", "click count must be in 1..5")
        target: dict[str, Any] = (
            {"kind": "current"}
            if x is None
            else {"kind": "root", "point": _point(x, y)}  # type: ignore[arg-type]
        )
        return self._lease.submit(
            {
                "type": "pointer_click",
                "target": target,
                "button": button,
                "count": count,
                "duration_ms": _milliseconds(duration, 10_000, "pointer duration"),
                "curve": "smooth",
                "pre_click_dwell_ms": 0,
                "press_duration_ms": 0,
                "inter_click_interval_ms": 100,
            }
        )

    def drag(
        self,
        x: int,
        y: int,
        *,
        button: str = "left",
        duration: float | None = None,
        relative: bool = False,
    ) -> CommandSubmission:
        target = {
            "kind": "relative" if relative else "root",
            "delta" if relative else "point": _point(x, y),
        }
        return self._lease.submit(
            {
                "type": "pointer_drag",
                "target": target,
                "button": button,
                "duration_ms": _milliseconds(duration, 10_000, "pointer duration"),
                "curve": "smooth",
                "press_dwell_ms": 0,
                "release_dwell_ms": 0,
            }
        )

    def scroll(
        self, direction: str, count: int = 1, *, interval: float = 0
    ) -> CommandSubmission:
        if direction not in {"up", "down", "left", "right"}:
            raise XenoteerError("invalid_request", "scroll direction is invalid")
        if isinstance(count, bool) or not isinstance(count, int) or not 1 <= count <= 1000:
            raise XenoteerError("invalid_request", "scroll count must be in 1..1000")
        interval_ms = _milliseconds(interval, 1000, "scroll interval")
        return self._lease.submit(
            {
                "type": "pointer_scroll",
                "direction": direction,
                "count": count,
                "interval_ms": interval_ms,
            }
        )


class Keyboard:
    """Physical keyboard and explicit text insertion API."""

    __slots__ = ("_lease",)

    def __init__(self, lease: "ControlLease") -> None:
        self._lease = lease

    def press(
        self, key: str | Mapping[str, Any], *, hold: float = 0
    ) -> CommandSubmission:
        return self._lease.submit(
            {
                "type": "keyboard_press",
                "key": _key(key),
                "hold_ms": _milliseconds(hold, 10_000, "key hold"),
            }
        )

    def chord(
        self, keys: list[str | Mapping[str, Any]], *, hold: float = 0
    ) -> CommandSubmission:
        if not 1 <= len(keys) <= 16:
            raise XenoteerError("invalid_request", "keyboard chord must have 1..16 keys")
        return self._lease.submit(
            {
                "type": "keyboard_chord",
                "keys": [_key(key) for key in keys],
                "hold_ms": _milliseconds(hold, 10_000, "key hold"),
            }
        )

    def insert_text(
        self,
        text: str,
        target: Mapping[str, Any],
        *,
        strategy: str = "auto",
        preserve_clipboard: bool = True,
        paste_timeout: float = 2.0,
        verify_length_only: bool = True,
    ) -> CommandSubmission:
        """Insert bounded inline text without putting content in diagnostics."""

        if not isinstance(text, str) or len(text.encode("utf-8")) > 256 * 1024:
            raise XenoteerError(
                "invalid_request", "inline text must be no larger than 256 KiB"
            )
        if not isinstance(verify_length_only, bool):
            raise XenoteerError(
                "invalid_request", "verify_length_only must be a bool"
            )
        target_wire = copy.deepcopy(dict(target))
        clipboard_options = None
        semantic_options = None
        auto_policy = None
        if strategy in {"clipboard", "auto"}:
            clipboard_options = {
                "preserve_clipboard": preserve_clipboard,
                "paste_observation_timeout_ms": max(
                    1,
                    _milliseconds(paste_timeout, 30_000, "paste timeout") or 1,
                ),
            }
        if strategy == "semantic" or (
            strategy == "auto" and target_wire.get("target") == "element"
        ):
            semantic_options = {
                "insertion_point": {"kind": "caret"},
                "selection": "collapse_after",
                "verify_length_only": verify_length_only,
                "postcondition": None,
            }
        if strategy == "auto" and target_wire.get("target") == "element":
            allowed = ["semantic"]
            if target_wire.get("window_fallback") is not None:
                allowed += ["physical", "clipboard", "physical_extended"]
            else:
                clipboard_options = None
            auto_policy = {
                "allowed_strategies": allowed,
                "fallback": "before_effect_only",
            }
        return self._lease.submit(
            {
                "type": "text_insert",
                "text": {"source": "inline", "text": text},
                "target": target_wire,
                "strategy": strategy,
                "clipboard_options": clipboard_options,
                "semantic_options": semantic_options,
                "auto_policy": auto_policy,
            }
        )

    def insert_artifact(
        self,
        artifact: ArtifactRef,
        target: Mapping[str, Any],
        *,
        strategy: str = "auto",
        preserve_clipboard: bool = True,
        paste_timeout: float = 2.0,
    ) -> CommandSubmission:
        """Insert a verified UTF-8 clipboard-input artifact without inlining it."""

        artifact.require_scope(
            self._lease._desktop.id,
            self._lease._desktop.generation,
            "clipboard_input",
        )
        if artifact.content_type != "text/plain;charset=utf-8":
            raise XenoteerError(
                "invalid_request", "text artifact must be text/plain;charset=utf-8"
            )
        target_wire = copy.deepcopy(dict(target))
        if strategy not in {"clipboard", "auto"}:
            raise XenoteerError(
                "invalid_request", "artifact text requires clipboard or auto strategy"
            )
        return self._lease.submit(
            {
                "type": "text_insert",
                "text": {"source": "artifact", "artifact": artifact.wire()},
                "target": target_wire,
                "strategy": strategy,
                "semantic_options": None,
                "clipboard_options": {
                    "preserve_clipboard": preserve_clipboard,
                    "paste_observation_timeout_ms": max(
                        1,
                        _milliseconds(paste_timeout, 30_000, "paste timeout") or 1,
                    ),
                },
                "auto_policy": (
                    None
                    if strategy != "auto"
                    else {
                        "allowed_strategies": ["clipboard"],
                        "fallback": "before_effect_only",
                    }
                ),
            }
        )


class ControlledClipboard:
    """Selection mutations gated by the same explicit controller lease."""

    __slots__ = ("_lease",)

    def __init__(self, lease: "ControlLease") -> None:
        self._lease = lease

    def set_text(
        self, text: str, *, selection: str = "clipboard"
    ) -> CommandSubmission:
        if not isinstance(text, str) or len(text.encode("utf-8")) > 256 * 1024:
            raise XenoteerError(
                "invalid_request", "inline clipboard text must be no larger than 256 KiB"
            )
        return self._lease.submit(
            {
                "type": "selection_set",
                "selection": selection,
                "content": {"source": "inline_text", "text": text},
            }
        )

    def set_artifact(
        self, artifact: ArtifactRef, *, selection: str = "clipboard"
    ) -> CommandSubmission:
        """Set a selection from one exact generation-bound input artifact."""

        artifact.require_scope(
            self._lease._desktop.id,
            self._lease._desktop.generation,
            "clipboard_input",
        )
        return self._lease.submit(
            {
                "type": "selection_set",
                "selection": selection,
                "content": {"source": "artifact", "artifact": artifact.wire()},
            }
        )

    def clear(self, *, selection: str = "clipboard") -> CommandSubmission:
        return self._lease.submit(
            {"type": "selection_clear", "selection": selection}
        )


class ControlLease:
    """Owned generation-bound lease; GC never claims asynchronous release."""

    __slots__ = (
        "_active",
        "_clipboard",
        "_desktop",
        "_keyboard",
        "_lock",
        "_mouse",
        "_renewal_error",
        "_renewal_task",
        "_state",
        "_status",
        "_transport",
        "_ttl_ms",
    )

    def __init__(
        self,
        desktop: "Desktop",
        transport: AsyncTransport,
        state: object,
        ttl_ms: int | None,
    ) -> None:
        self._desktop = desktop
        self._transport = transport
        self._state = _validate_state(state, desktop, owned=True)
        self._ttl_ms = ttl_ms
        self._active = True
        self._status = "active"
        self._lock = asyncio.Lock()
        self._renewal_task: asyncio.Task[None] | None = None
        self._renewal_error: XenoteerError | None = None
        self._mouse = Mouse(self)
        self._keyboard = Keyboard(self)
        self._clipboard = ControlledClipboard(self)

    @property
    def id(self) -> str:
        self._ensure_active()
        return cast(str, self._state["lease_id"])

    @property
    def expires_at(self) -> str:
        self._ensure_active()
        return cast(str, self._state["expires_at"])

    @property
    def active(self) -> bool:
        return self._active and self._status == "active"

    @property
    def requires_cleanup(self) -> bool:
        """Whether this capability may still be owned, including ambiguity."""

        return self._active and self._status not in {"released", "revoked"}

    @property
    def mouse(self) -> Mouse:
        return self._mouse

    @property
    def keyboard(self) -> Keyboard:
        return self._keyboard

    @property
    def clipboard(self) -> ControlledClipboard:
        return self._clipboard

    def _ensure_active(self) -> None:
        self._desktop.registry.require_current()
        if self._renewal_error is not None:
            raise XenoteerError(
                "lease_renewal_failed",
                "controller lease renewal failed; recover or reacquire control",
                source=self._renewal_error,
            )
        if not self._active or self._status in {"released", "revoked"}:
            raise XenoteerError("lease_released", "controller lease was released")
        if self._status == "ambiguous":
            raise XenoteerError(
                "lease_ambiguous",
                "controller lease ownership is ambiguous; call recover()",
            )

    def submit(
        self,
        command: Mapping[str, Any],
        *,
        command_id: str | None = None,
        deadline: str | None = None,
        _lifecycle: "ReferenceLifecycle | None" = None,
    ) -> CommandSubmission:
        self._ensure_active()
        return self._desktop.submit(
            command,
            command_id=command_id,
            lease_id=self.id,
            deadline=deadline,
            _lifecycle=_lifecycle,
        )

    def reset_input(self) -> CommandSubmission:
        """Conservatively release only input owned by this Xenoteer lease."""

        return self.submit({"type": "input_reset"})

    async def renew(self) -> dict[str, Any]:
        async with self._lock:
            self._ensure_active()
            lease_id = self.id
            body: dict[str, Any] = {
                "protocol_version": self._desktop.protocol,
                "request_id": _request_id(),
                "desktop_id": self._desktop.id,
                "desktop_generation": self._desktop.generation,
                "lease_id": lease_id,
            }
            if self._ttl_ms is not None:
                body["ttl_ms"] = self._ttl_ms
            path = (
                f"/v1/desktops/{quote(self._desktop.id, safe='')}/lease/"
                f"{quote(lease_id, safe='')}/renew"
            )
            try:
                wire = await self._transport.request("POST", path, body)
                state = _validate_state(wire, self._desktop, owned=True)
            except XenoteerError as error:
                self._status = "ambiguous"
                raise XenoteerError(
                    "lease_ambiguous",
                    "lease renewal outcome is ambiguous; call recover()",
                    source=error,
                ) from None
            if state["lease_id"] != lease_id:
                self._status = "revoked"
                raise XenoteerError(
                    "invalid_response", "lease renewal changed capability identity"
                )
            self._state = state
            return copy.deepcopy(state)

    async def release(self) -> dict[str, Any]:
        await self._stop_renewal()
        async with self._lock:
            self._desktop.registry.require_current()
            if not self._active or self._status in {"released", "revoked"}:
                raise XenoteerError("lease_released", "controller lease was released")
            lease_id = cast(str, self._state["lease_id"])
            body = {
                "protocol_version": self._desktop.protocol,
                "request_id": _request_id(),
                "desktop_id": self._desktop.id,
                "desktop_generation": self._desktop.generation,
                "lease_id": lease_id,
            }
            path = (
                f"/v1/desktops/{quote(self._desktop.id, safe='')}/lease/"
                f"{quote(lease_id, safe='')}"
            )
            try:
                wire = await self._transport.request("DELETE", path, body)
                state = _validate_state(wire, self._desktop, owned=False)
            except XenoteerError as error:
                self._status = "ambiguous"
                raise XenoteerError(
                    "lease_ambiguous",
                    "lease release outcome is ambiguous; call recover()",
                    source=error,
                ) from None
            self._state = state
            self._active = False
            self._status = "released"
            return copy.deepcopy(state)

    async def recover(self) -> dict[str, Any]:
        """Query authoritative ownership after an ambiguous renew or release."""

        async with self._lock:
            self._desktop.registry.require_current()
            response = await self._desktop.control_state()
            state = response.get("state")
            lease_id = response.get("lease_id")
            if state == "held_by_caller" and lease_id == self._state.get("lease_id"):
                self._state = copy.deepcopy(response)
                self._status = "active"
                self._renewal_error = None
            else:
                self._active = False
                self._status = "revoked"
            return copy.deepcopy(response)

    def _start_renewal(self) -> None:
        if self._renewal_task is not None or not self.active:
            return
        interval = (
            30.0
            if self._ttl_ms is None
            else max(0.25, min(60.0, self._ttl_ms / 2000))
        )
        self._renewal_task = asyncio.create_task(
            self._renewal_loop(interval), name="xenoteer-lease-renewal"
        )

    async def _renewal_loop(self, interval: float) -> None:
        try:
            while self.active:
                await asyncio.sleep(interval)
                if not self.active:
                    break
                try:
                    await self.renew()
                except XenoteerError as error:
                    self._renewal_error = error
                    break
        except asyncio.CancelledError:
            pass

    async def _stop_renewal(self) -> None:
        task = self._renewal_task
        self._renewal_task = None
        if task is None or task is asyncio.current_task():
            return
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

    async def __aenter__(self) -> "ControlLease":
        self._ensure_active()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        if self._active:
            release_task = asyncio.create_task(self.release())
            try:
                await asyncio.shield(release_task)
            except asyncio.CancelledError:
                release_task.add_done_callback(_consume_release_failure)
                raise
            except XenoteerError as error:
                if exc is None:
                    raise
                exc.add_note(f"controller lease cleanup failed: {error.code}")

    def __repr__(self) -> str:
        lease = self._state.get("lease_id") if self._active else "<released>"
        return (
            f"ControlLease(id={lease!r}, generation={self._desktop.generation!r}, "
            f"active={self._active})"
        )


class ControlContext:
    """Lazy async scope used by ``async with desktop.control()``."""

    __slots__ = ("_desktop", "_lease", "_ttl")

    def __init__(self, desktop: "Desktop", ttl: float | None) -> None:
        self._desktop = desktop
        self._ttl = ttl
        self._lease: ControlLease | None = None

    async def __aenter__(self) -> ControlLease:
        self._lease = await self._desktop.acquire_control(self._ttl)
        self._lease._start_renewal()
        return self._lease

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        if self._lease is not None and self._lease._active:
            release_task = asyncio.create_task(self._lease.release())
            try:
                await asyncio.shield(release_task)
            except asyncio.CancelledError:
                release_task.add_done_callback(_consume_release_failure)
                raise
            except XenoteerError as error:
                if exc is None:
                    raise
                exc.add_note(f"controller lease cleanup failed: {error.code}")


def _consume_release_failure(task: asyncio.Task[dict[str, Any]]) -> None:
    if not task.cancelled():
        task.exception()


if TYPE_CHECKING:
    from .desktop import Desktop
    from .policy import ReferenceLifecycle
