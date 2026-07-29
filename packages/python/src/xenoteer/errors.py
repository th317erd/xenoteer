# SPDX-License-Identifier: Apache-2.0
"""Stable, redaction-safe SDK failures."""

from __future__ import annotations

import copy
import json
from collections.abc import Mapping
from typing import Any


class XenoteerError(Exception):
    """Stable structured failure without request bodies or bearer material."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        status: int | None = None,
        request_id: str | None = None,
        command_id: str | None = None,
        problem_code: str | None = None,
        retry: str | None = None,
        effect_stage: str | None = None,
        cleanup: Mapping[str, Any] | None = None,
        details: Mapping[str, Any] | None = None,
        desktop_generation: str | None = None,
        source: BaseException | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.status = status
        self.request_id = request_id
        self.command_id = command_id
        self.problem_code = problem_code
        self.retry = retry
        self.effect_stage = effect_stage
        self.cleanup = _safe_mapping(cleanup)
        self.details = _safe_mapping(details)
        self.desktop_generation = desktop_generation
        self.source_type = None if source is None else type(source).__name__

    def __repr__(self) -> str:
        fields = [f"code={self.code!r}", f"message={str(self)!r}"]
        for name in (
            "status",
            "request_id",
            "command_id",
            "problem_code",
            "retry",
            "effect_stage",
            "cleanup",
            "details",
            "desktop_generation",
            "source_type",
        ):
            value = getattr(self, name)
            if value is not None and value != {}:
                fields.append(f"{name}={value!r}")
        return f"XenoteerError({', '.join(fields)})"


def error_from_problem(status: int, problem: Mapping[str, Any]) -> XenoteerError:
    """Validate and project bounded, pre-redacted RFC 9457 problem fields."""

    def optional_text(name: str) -> str | None:
        value = problem.get(name)
        if not isinstance(value, str) or not value or len(value.encode("utf-8")) > 256:
            return None
        if any(ord(character) < 0x20 for character in value):
            return None
        return value

    problem_code = optional_text("code")
    category = (
        "authentication"
        if status == 401
        else "permission"
        if status == 403
        else _problem_category(problem_code)
    )
    retry = optional_text("retry")
    effect_stage = optional_text("effect_stage")
    details = problem.get("details")
    if not isinstance(details, Mapping):
        details = None
    desktop_generation = optional_text("desktop_generation")
    return XenoteerError(
        category,
        f"Xenoteer request failed with HTTP {status}",
        status=status,
        request_id=optional_text("request_id"),
        command_id=optional_text("command_id"),
        problem_code=problem_code,
        retry=retry,
        effect_stage=effect_stage,
        cleanup=(
            details.get("cleanup")
            if isinstance(details, Mapping)
            and isinstance(details.get("cleanup"), Mapping)
            else None
        ),
        details=details,
        desktop_generation=desktop_generation,
    )


def _problem_category(code: str | None) -> str:
    if code is None:
        return "unexpected_http_status"
    if code in {"invalid_token", "authentication"}:
        return "authentication"
    if code in {"forbidden", "permission_denied", "permission"}:
        return "permission"
    if code in {"stale_reference", "desktop_generation_mismatch"}:
        return "stale_reference"
    if code in {"ambiguous_target", "conflict", "command_body_conflict"}:
        return "conflict"
    if code in {"not_found", "command_not_found"}:
        return "not_found"
    if "timeout" in code or "deadline" in code:
        return "timeout"
    if "lease" in code:
        return "lease"
    if code in {"backpressure", "resource_exhausted", "rate_limited"}:
        return "resource"
    if code.startswith("invalid_") or code in {"unsupported_version", "unsupported"}:
        return "validation"
    return "server"


def _safe_mapping(value: Mapping[str, Any] | None) -> dict[str, Any]:
    if value is None:
        return {}
    if len(value) > 16:
        return {}
    copied: dict[str, Any] = {}
    for key, item in value.items():
        if (
            not isinstance(key, str)
            or not key
            or len(key.encode("utf-8")) > 64
            or any(
                not (
                    character.isascii()
                    and (character.islower() or character.isdigit() or character in "._-")
                )
                for character in key
            )
        ):
            return {}
        copied[key] = copy.deepcopy(item)
    try:
        encoded = json.dumps(copied, ensure_ascii=False, allow_nan=False).encode("utf-8")
    except (TypeError, ValueError, RecursionError):
        return {}
    return copied if len(encoded) <= 8192 else {}
