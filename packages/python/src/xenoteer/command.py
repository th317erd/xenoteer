# SPDX-License-Identifier: Apache-2.0
"""Cancel-safe handles for accepted, deduplicated commands."""

from __future__ import annotations

import asyncio
import copy
import json
from collections.abc import Mapping
from collections.abc import Generator
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any
from urllib.parse import quote

from .errors import XenoteerError
from .protocol_generated import CommandResultWire
from .transport import AsyncTransport, request_with_deadline
from .wire import canonicalize_uint64_fields, validate_uint64_fields


_TERMINAL = frozenset(
    {
        "succeeded",
        "failed",
        "cancelled_before_effect",
        "cancelled_after_effect",
        "deadline_before_effect",
        "deadline_after_effect",
    }
)
_LIFECYCLES = _TERMINAL | {"accepted", "running"}
_NO_VISIBLE_EFFECT = frozenset({"none", "accepted"})
_KNOWN_OUTCOMES = frozenset(
    {
        "probe",
        "application_launched",
        "process_status",
        "process_terminated",
        "acknowledged",
        "window_control",
        "text_inserted",
        "element_action",
        "element_physical_click",
    }
)
_ENVELOPE_FIELDS = frozenset(
    {
        "protocol_version",
        "request_id",
        "command_id",
        "desktop_id",
        "desktop_generation",
        "lease_id",
        "deadline",
        "trace_policy",
        "command",
    }
)
_AUTHORITY_REFERENCE_FIELDS: dict[str, frozenset[str]] = {
    "process": frozenset(
        {"desktop_generation", "pid", "proc_start_ticks", "launch_id"}
    ),
    "window": frozenset(
        {
            "desktop_id",
            "desktop_generation",
            "xid",
            "observed_generation",
            "identity_hash",
        }
    ),
    "element": frozenset(
        {
            "desktop_id",
            "desktop_generation",
            "atspi_generation",
            "application",
            "object_path",
            "object_identity_hash",
            "cache_sequence",
        }
    ),
    "application": frozenset(
        {
            "desktop_id",
            "desktop_generation",
            "atspi_generation",
            "unique_bus_name",
            "root_object_path",
            "app_instance_generation",
            "identity_hash",
        }
    ),
    "artifact": frozenset(
        {
            "artifact_id",
            "purpose",
            "desktop_id",
            "desktop_generation",
            "content_type",
            "content_length",
            "sha256",
            "created_at",
            "expires_at",
        }
    ),
}


@dataclass(frozen=True, slots=True)
class TerminalEffect:
    """Stable retry-oriented projection of a terminal command result."""

    category: str
    effect_stage: str
    visible_effect: bool
    retry_requires_explicit_user_decision: bool
    _raw: dict[str, Any] = field(repr=False)

    @property
    def raw(self) -> dict[str, Any]:
        return copy.deepcopy(self._raw)


def classify_terminal_effect(value: Mapping[str, Any]) -> TerminalEffect:
    """Classify terminal effect evidence without erasing detailed result fields."""

    lifecycle = value.get("lifecycle")
    effect_stage = value.get("effect_stage")
    if lifecycle not in _TERMINAL or not isinstance(effect_stage, str):
        raise XenoteerError("invalid_response", "command result is not terminal")
    visible = effect_stage not in _NO_VISIBLE_EFFECT
    if lifecycle == "succeeded":
        category = "success"
    elif lifecycle.startswith("cancelled_"):
        category = "cancelled"
    elif lifecycle.startswith("deadline_"):
        category = "timeout"
    elif lifecycle == "failed" and visible:
        category = "partial_effect"
    else:
        category = "failure"
    return TerminalEffect(
        category=category,
        effect_stage=effect_stage,
        visible_effect=visible,
        retry_requires_explicit_user_decision=visible and category != "success",
        _raw=copy.deepcopy(dict(value)),
    )


def validate_client_command_envelope(value: object) -> dict[str, Any]:
    """Strictly decode one client-authored v1 command envelope.

    Server outputs remain additive, but client inputs and nested
    authority-bearing references reject unknown fields.
    """

    if not isinstance(value, Mapping) or not all(
        isinstance(key, str) for key in value
    ):
        raise XenoteerError("invalid_request", "command envelope must be an object")
    unknown = set(value) - _ENVELOPE_FIELDS
    required = {
        "protocol_version",
        "request_id",
        "command_id",
        "desktop_id",
        "desktop_generation",
        "command",
    }
    if unknown or not required <= set(value):
        raise XenoteerError(
            "invalid_request", "command envelope contains unknown or missing fields"
        )
    command = value.get("command")
    if not isinstance(command, Mapping) or not isinstance(command.get("type"), str):
        raise XenoteerError("invalid_request", "command envelope command is invalid")
    trace_policy = value.get("trace_policy")
    if trace_policy is not None and trace_policy not in {
        "none",
        "normal",
        "detailed",
    }:
        raise XenoteerError("invalid_request", "command trace policy is invalid")
    _validate_authority_references(command)
    try:
        normalized = canonicalize_uint64_fields(value)
    except (TypeError, ValueError):
        raise XenoteerError(
            "invalid_request", "command envelope contains an invalid uint64 value"
        ) from None
    if not isinstance(normalized, dict):
        raise XenoteerError("invalid_request", "command envelope is invalid")
    return normalized


def _validate_authority_references(value: object) -> None:
    if isinstance(value, Mapping):
        for key, child in value.items():
            allowed = _AUTHORITY_REFERENCE_FIELDS.get(key)
            if allowed is not None and isinstance(child, Mapping):
                if set(child) != allowed:
                    raise XenoteerError(
                        "invalid_request",
                        f"{key} reference contains unknown or missing identity fields",
                    )
            _validate_authority_references(child)
    elif isinstance(value, list):
        for child in value:
            _validate_authority_references(child)


class CommandSubmission:
    """Pre-I/O exact command identity and canonical body.

    Construction performs no I/O. The caller can retain ``id`` and
    ``canonical_body`` across cancellation or disconnect, look up that ID, and
    explicitly call ``send`` again only after proving the ledger has no entry.
    """

    __slots__ = (
        "_canonical_body",
        "_command_id",
        "_desktop_generation",
        "_desktop_id",
        "_envelope",
        "_artifacts",
        "_cleanup_refs",
        "_lifecycle",
        "_registry",
        "_transport",
        "_type",
    )

    def __init__(
        self,
        transport: AsyncTransport,
        desktop_id: str,
        desktop_generation: str,
        envelope: Mapping[str, Any],
        *,
        registry: "GenerationRegistry | None" = None,
        lifecycle: "ReferenceLifecycle | None" = None,
        artifacts: "Artifacts | None" = None,
    ) -> None:
        command_id = envelope.get("command_id")
        command = envelope.get("command")
        if not isinstance(command_id, str) or not isinstance(command, Mapping):
            raise XenoteerError("invalid_request", "command envelope is invalid")
        self._transport = transport
        self._desktop_id = desktop_id
        self._desktop_generation = desktop_generation
        self._registry = registry
        self._lifecycle = lifecycle
        self._artifacts = artifacts
        self._cleanup_refs = _clipboard_input_artifacts(envelope)
        self._command_id = command_id
        self._type = command.get("type") if isinstance(command.get("type"), str) else "<unknown>"
        self._envelope = copy.deepcopy(dict(envelope))
        self._canonical_body = json.dumps(
            self._envelope,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")

    @property
    def id(self) -> str:
        return self._command_id

    @property
    def desktop_generation(self) -> str:
        return self._desktop_generation

    @property
    def envelope(self) -> dict[str, Any]:
        """Return a defensive copy for explicit lookup/resend workflows."""

        return copy.deepcopy(self._envelope)

    @property
    def canonical_body(self) -> bytes:
        """Return exact deterministic bytes without consuming retained state."""

        return bytes(self._canonical_body)

    async def send(self) -> "CommandHandle":
        """Perform one submission attempt; this method has no retry loop."""

        if self._registry is not None:
            self._registry.require_current()
        path = f"/v1/desktops/{quote(self._desktop_id, safe='')}/commands"
        try:
            response = await self._transport.request(
                "POST",
                path,
                self.envelope,
                headers={"idempotency-key": self._command_id},
            )
        except XenoteerError as error:
            self._invalidate_from_error(error)
            raise
        if response.get("command_id") != self._command_id:
            raise XenoteerError(
                "invalid_response", "command response ID did not match submission"
            )
        handle = CommandHandle(
            self._transport,
            self._desktop_id,
            self._desktop_generation,
            response,
            registry=self._registry,
            lifecycle=self._lifecycle,
            artifacts=self._artifacts,
            cleanup_refs=self._cleanup_refs,
        )
        await handle._cleanup_if_terminal()
        return handle

    def _invalidate_from_error(self, error: XenoteerError) -> None:
        if error.code == "stale_reference" or error.problem_code == "stale_reference":
            if self._lifecycle is not None:
                self._lifecycle.invalidate("server_stale_reference")
        if error.code == "generation_changed" or error.problem_code in {
            "desktop_generation_mismatch",
            "generation_changed",
        }:
            if self._registry is not None:
                self._registry.invalidate("server_generation_changed")

    def __await__(self) -> Generator[Any, None, "CommandHandle"]:
        return self.send().__await__()

    def __repr__(self) -> str:
        return (
            f"CommandSubmission(id={self._command_id!r}, type={self._type!r}, "
            "body=<redacted>)"
        )


def validate_command_result(value: object, command_id: str) -> CommandResultWire:
    """Validate stable required fields while retaining additive response data."""

    if not isinstance(value, dict):
        raise XenoteerError("invalid_response", "command result must be an object")
    if (
        value.get("command_id") != command_id
        or value.get("lifecycle") not in _LIFECYCLES
        or not isinstance(value.get("effect_stage"), str)
        or not isinstance(value.get("accepted_at"), str)
        or not isinstance(value.get("warnings"), list)
    ):
        raise XenoteerError("invalid_response", "server returned an invalid command result")
    outcome = value.get("outcome")
    if isinstance(outcome, Mapping) and outcome.get("type") not in _KNOWN_OUTCOMES:
        raise XenoteerError(
            "unsupported_response_variant",
            "server returned an unsupported command outcome",
        )
    try:
        validate_uint64_fields(value)
    except (TypeError, ValueError):
        raise XenoteerError(
            "invalid_response", "command result contains an invalid uint64 wire value"
        ) from None
    return copy.deepcopy(value)  # type: ignore[return-value]


class CommandHandle:
    """Generation-bound command identity that never replays work implicitly."""

    __slots__ = (
        "_command_id",
        "_desktop_generation",
        "_desktop_id",
        "_artifacts",
        "_cleanup_done",
        "_cleanup_refs",
        "_latest",
        "_lifecycle",
        "_registry",
        "_transport",
    )

    def __init__(
        self,
        transport: AsyncTransport,
        desktop_id: str,
        desktop_generation: str,
        initial: Mapping[str, Any],
        *,
        registry: "GenerationRegistry | None" = None,
        lifecycle: "ReferenceLifecycle | None" = None,
        artifacts: "Artifacts | None" = None,
        cleanup_refs: tuple["ArtifactRef", ...] = (),
    ) -> None:
        command_id = initial.get("command_id")
        if not isinstance(command_id, str):
            raise XenoteerError("invalid_response", "command response has no command ID")
        self._transport = transport
        self._desktop_id = desktop_id
        self._desktop_generation = desktop_generation
        self._registry = registry
        self._lifecycle = lifecycle
        self._artifacts = artifacts
        self._cleanup_refs = cleanup_refs
        self._cleanup_done = False
        self._command_id = command_id
        self._latest = validate_command_result(dict(initial), command_id)

    @property
    def id(self) -> str:
        return self._command_id

    @property
    def desktop_generation(self) -> str:
        return self._desktop_generation

    @property
    def latest(self) -> CommandResultWire:
        return copy.deepcopy(self._latest)

    @property
    def terminal(self) -> bool:
        return self._latest["lifecycle"] in _TERMINAL

    def _path(self) -> str:
        return (
            f"/v1/desktops/{quote(self._desktop_id, safe='')}"
            f"/commands/{quote(self._command_id, safe='')}"
        )

    async def refresh(self) -> CommandResultWire:
        """Read the ledger entry without submitting or replaying a command."""

        self._require_current()
        try:
            response = await self._transport.request("GET", self._path())
        except XenoteerError as error:
            self._invalidate_from_error(error)
            raise
        self._latest = validate_command_result(response, self._command_id)
        await self._cleanup_if_terminal()
        return self.latest

    async def wait_once(self, timeout: float = 30.0) -> CommandResultWire:
        """Perform one server-side wait; local cancellation doesn't cancel work."""

        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not 0 < timeout <= 30
        ):
            raise XenoteerError("invalid_request", "command wait must be in (0, 30] seconds")
        timeout_ms = max(1, min(30_000, int(timeout * 1000)))
        self._require_current()
        try:
            response = await request_with_deadline(
                self._transport,
                "GET",
                f"{self._path()}/wait?timeout_ms={timeout_ms}",
                timeout=timeout_ms / 1_000 + 5,
            )
        except XenoteerError as error:
            self._invalidate_from_error(error)
            raise
        self._latest = validate_command_result(response, self._command_id)
        await self._cleanup_if_terminal()
        return self.latest

    async def wait_until_terminal(self, timeout: float = 3600.0) -> CommandResultWire:
        """Await this identity through reads only; never re-submit its mutation."""

        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not 0 < timeout <= 3600
        ):
            raise XenoteerError(
                "invalid_request", "overall command wait must be in (0, 3600] seconds"
            )
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while not self.terminal:
            remaining = deadline - loop.time()
            if remaining <= 0:
                raise XenoteerError(
                    "command_wait_timeout",
                    "local command wait timed out; server command was not cancelled",
                )
            await self.wait_once(min(remaining, 30.0))
        await self._cleanup_if_terminal()
        if self._latest["lifecycle"] != "succeeded":
            raise _terminal_error(self._latest, self._command_id)
        return self.latest

    async def cancel(self) -> CommandResultWire:
        """Explicitly request cooperative server cancellation for this exact ID."""

        self._require_current()
        try:
            response = await self._transport.request("DELETE", self._path())
        except XenoteerError as error:
            self._invalidate_from_error(error)
            raise
        self._latest = validate_command_result(response, self._command_id)
        await self._cleanup_if_terminal()
        return self.latest

    def _require_current(self) -> None:
        if self._registry is not None:
            self._registry.require_current()
        if self._lifecycle is not None:
            self._lifecycle.require_current()

    def _invalidate_from_error(self, error: XenoteerError) -> None:
        if error.code == "stale_reference" or error.problem_code == "stale_reference":
            if self._lifecycle is not None:
                self._lifecycle.invalidate("server_stale_reference")
        if error.code == "generation_changed" or error.problem_code in {
            "desktop_generation_mismatch",
            "generation_changed",
        }:
            if self._registry is not None:
                self._registry.invalidate("server_generation_changed")

    async def _cleanup_if_terminal(self) -> None:
        if (
            self._cleanup_done
            or not self.terminal
            or self._artifacts is None
            or not self._cleanup_refs
        ):
            return
        self._cleanup_done = True
        for artifact in self._cleanup_refs:
            try:
                await self._artifacts.delete(artifact)
            except Exception:
                pass

    def __await__(self) -> Generator[Any, None, CommandResultWire]:
        return self.wait_until_terminal().__await__()

    def __repr__(self) -> str:
        return (
            f"CommandHandle(id={self._command_id!r}, "
            f"desktop_generation={self._desktop_generation!r}, "
            f"lifecycle={self._latest['lifecycle']!r})"
        )


def _terminal_error(result: Mapping[str, Any], command_id: str) -> XenoteerError:
    problem = result.get("problem")
    retry = None
    details = None
    problem_code = None
    if isinstance(problem, Mapping):
        retry = problem.get("retry") if isinstance(problem.get("retry"), str) else None
        details = problem.get("details") if isinstance(problem.get("details"), Mapping) else None
        problem_code = problem.get("code") if isinstance(problem.get("code"), str) else None
    cleanup = result.get("cleanup")
    return XenoteerError(
        "command_failed",
        f"command reached terminal lifecycle {result.get('lifecycle')}",
        command_id=command_id,
        problem_code=problem_code,
        retry=retry,
        effect_stage=(
            result.get("effect_stage")
            if isinstance(result.get("effect_stage"), str)
            else None
        ),
        cleanup=cleanup if isinstance(cleanup, Mapping) else None,
        details=details,
    )


def _clipboard_input_artifacts(
    value: object,
) -> tuple["ArtifactRef", ...]:
    from .artifacts import ArtifactRef

    found: dict[str, ArtifactRef] = {}

    def visit(node: object) -> None:
        if isinstance(node, Mapping):
            if node.get("purpose") == "clipboard_input" and "artifact_id" in node:
                try:
                    artifact = ArtifactRef.from_wire(node, purpose="clipboard_input")
                except XenoteerError:
                    pass
                else:
                    found[artifact.artifact_id] = artifact
            for child in node.values():
                visit(child)
        elif isinstance(node, list):
            for child in node:
                visit(child)

    visit(value)
    return tuple(found.values())


if TYPE_CHECKING:
    from .artifacts import ArtifactRef, Artifacts
    from .policy import ReferenceLifecycle
    from .state import GenerationRegistry
