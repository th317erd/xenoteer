# SPDX-License-Identifier: Apache-2.0
"""Shared generation fencing for every object derived from one connection."""

from __future__ import annotations

from .errors import XenoteerError


class GenerationRegistry:
    """One monotonic invalidation latch shared by all generation-bound handles."""

    __slots__ = ("_generation", "_reason")

    def __init__(self, generation: str) -> None:
        self._generation = generation
        self._reason: str | None = None

    @property
    def generation(self) -> str:
        return self._generation

    @property
    def stale(self) -> bool:
        return self._reason is not None

    @property
    def reason(self) -> str | None:
        return self._reason

    def invalidate(self, reason: str) -> None:
        if self._reason is None:
            self._reason = reason

    def observe(self, generation: str) -> None:
        if generation != self._generation:
            self.invalidate("desktop_generation_changed")

    def require_current(self) -> None:
        if self._reason is not None:
            raise XenoteerError(
                "generation_changed",
                "desktop generation changed; reacquire status and every derived handle",
                desktop_generation=self._generation,
            )
