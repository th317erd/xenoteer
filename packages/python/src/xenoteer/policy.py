# SPDX-License-Identifier: Apache-2.0
"""Pure recovery, continuity, and reference-lifecycle policy helpers."""

from __future__ import annotations

import copy
from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

from .errors import XenoteerError


class RecoveryDecision(str, Enum):
    """The only safe next steps after an interrupted command exchange."""

    LOOKUP_SAME_ID = "lookup_same_id"
    ATTACH_EXISTING = "attach_existing"
    RESUBMIT_EXACT = "resubmit_exact"
    OUTCOME_UNKNOWN = "outcome_unknown"
    GENERATION_CHANGED = "generation_changed"


@dataclass(slots=True)
class CommandRecoveryPolicy:
    """Stateful, retry-neutral policy for one exact command identity."""

    command_id: str
    canonical_body: bytes
    desktop_generation: str
    accepted: bool = False
    visible_effect: bool = False
    disconnected: bool = False
    local_wait_cancelled: bool = False
    body_conflict: bool = False
    generation_changed: bool = False
    _decision: RecoveryDecision | None = field(default=None, repr=False)

    def mark_accepted(self) -> None:
        self.accepted = True

    def mark_visible_effect(self) -> None:
        self.visible_effect = True

    def mark_disconnect(self) -> None:
        """Record ambiguity without creating or replaying any work."""

        self.disconnected = True
        self._decision = None

    def reconnect(self, desktop_generation: str) -> RecoveryDecision:
        if desktop_generation != self.desktop_generation:
            self.generation_changed = True
            self._decision = RecoveryDecision.GENERATION_CHANGED
        else:
            self._decision = RecoveryDecision.LOOKUP_SAME_ID
        return self._decision

    def ledger_lookup(self, *, found: bool) -> RecoveryDecision:
        if self.generation_changed:
            raise XenoteerError(
                "generation_changed", "old command cannot be looked up in a new generation"
            )
        if self._decision != RecoveryDecision.LOOKUP_SAME_ID:
            raise XenoteerError(
                "invalid_recovery", "ledger lookup is not the current recovery step"
            )
        if found:
            self._decision = RecoveryDecision.ATTACH_EXISTING
        elif not self.accepted and not self.visible_effect:
            self._decision = RecoveryDecision.RESUBMIT_EXACT
        else:
            self._decision = RecoveryDecision.OUTCOME_UNKNOWN
        return self._decision

    def validate_resubmission(self, command_id: str, canonical_body: bytes) -> None:
        """Permit only byte-equivalent same-ID work after a proven miss."""

        if command_id != self.command_id:
            raise XenoteerError(
                "command_identity", "recovery cannot allocate a fresh command ID"
            )
        if canonical_body != self.canonical_body:
            self.body_conflict = True
            raise XenoteerError(
                "command_body_conflict",
                "command ID was reused with a different canonical body",
            )
        if self._decision != RecoveryDecision.RESUBMIT_EXACT:
            raise XenoteerError(
                "unsafe_replay", "command is not proven safe for exact resubmission"
            )

    def cancel_local_wait(self) -> None:
        """Cancel only the local waiter; server cancellation remains explicit."""

        self.local_wait_cancelled = True

    @property
    def decision(self) -> RecoveryDecision | None:
        return self._decision

    @property
    def automatic_replay_allowed(self) -> bool:
        return False


@dataclass(slots=True)
class EventContinuityPolicy:
    """Bounded visible-event cursor and explicit continuity-barrier state."""

    capacity: int
    desktop_generation: str | None = None
    resume_cursor: int | None = None
    subscription_active: bool = True
    refresh_required: bool = False
    generation_handles_valid: bool = True
    resync_reason: str | None = None
    delivered: list[int] = field(default_factory=list)
    _queued: list[int] = field(default_factory=list, repr=False)

    def __post_init__(self) -> None:
        if (
            isinstance(self.capacity, bool)
            or not isinstance(self.capacity, int)
            or not 1 <= self.capacity <= 4096
        ):
            raise XenoteerError("invalid_request", "event capacity must be in 1..4096")

    @property
    def queued_count(self) -> int:
        return len(self._queued)

    def consume_one(self) -> int:
        if not self._queued:
            raise XenoteerError("invalid_state", "event queue is empty")
        return self._queued.pop(0)

    def receive_visible(self, sequence: int, desktop_generation: str) -> bool:
        """Deliver visible sequences once; gaps are never inferred as loss."""

        if not self.subscription_active:
            raise XenoteerError("resync_required", "event subscription needs refresh")
        if self.desktop_generation is None:
            self.desktop_generation = desktop_generation
        elif desktop_generation != self.desktop_generation:
            self.require_resync("generation_changed")
            return False
        if (
            isinstance(sequence, bool)
            or not isinstance(sequence, int)
            or sequence <= 0
        ):
            raise XenoteerError("invalid_response", "event sequence is invalid")
        if sequence in self.delivered:
            return False
        if self.resume_cursor is not None and sequence <= self.resume_cursor:
            return False
        if len(self._queued) >= self.capacity:
            self.require_resync("outbound_backpressure")
            return False
        self._queued.append(sequence)
        self.delivered.append(sequence)
        self.resume_cursor = sequence
        return True

    def replay_complete(self, through_sequence: int, desktop_generation: str) -> None:
        if desktop_generation != self.desktop_generation:
            self.require_resync("generation_changed")
            return
        if (
            isinstance(through_sequence, bool)
            or not isinstance(through_sequence, int)
            or through_sequence < 0
            or (
                self.resume_cursor is not None
                and through_sequence < self.resume_cursor
            )
        ):
            raise XenoteerError("invalid_response", "replay boundary is invalid")
        self.resume_cursor = through_sequence

    def require_resync(self, reason: str) -> None:
        if reason not in {
            "generation_changed",
            "history_lost",
            "sequence_ahead",
            "subscriber_lag",
            "outbound_backpressure",
        }:
            raise XenoteerError("invalid_response", "resync reason is invalid")
        self.subscription_active = False
        self.refresh_required = True
        self.resync_reason = reason
        if reason == "generation_changed":
            self.generation_handles_valid = False


class ReferenceLifecycle:
    """Identity-preserving stale marker shared by Window and Element handles."""

    __slots__ = ("_identity", "_registry", "_stale_reason")

    def __init__(
        self,
        identity: Mapping[str, Any],
        registry: "GenerationRegistry | None" = None,
    ) -> None:
        self._identity = copy.deepcopy(dict(identity))
        self._registry = registry
        self._stale_reason: str | None = None

    @property
    def identity(self) -> dict[str, Any]:
        return copy.deepcopy(self._identity)

    @property
    def stale(self) -> bool:
        return self._stale_reason is not None or (
            self._registry is not None and self._registry.stale
        )

    @property
    def stale_reason(self) -> str | None:
        return self._stale_reason

    def invalidate(self, reason: str) -> None:
        if not isinstance(reason, str) or not reason:
            raise XenoteerError("invalid_request", "stale reason must not be empty")
        if self._stale_reason is None:
            self._stale_reason = reason

    def require_current(self) -> None:
        if self._registry is not None:
            self._registry.require_current()
        if self._stale_reason is not None:
            raise XenoteerError(
                "stale_reference",
                "generation- or birth-bound handle is stale; explicitly relocate it",
            )

    def relocate(self, identity: Mapping[str, Any]) -> "ReferenceLifecycle":
        """Create a fresh handle state; never mutate or retarget the old identity."""

        return ReferenceLifecycle(identity, self._registry)


if TYPE_CHECKING:
    from .state import GenerationRegistry
