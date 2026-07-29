# SPDX-License-Identifier: Apache-2.0
"""Generated-shape public v1 wire types.

This intentionally contains public interface declarations only. It is kept
small enough to audit and never imports the separately licensed server.
"""

from __future__ import annotations

from typing import NewType, Required, TypeAlias, TypedDict


CanonicalUInt64 = NewType("CanonicalUInt64", str)
JsonScalar: TypeAlias = None | bool | int | float | str
JsonValue: TypeAlias = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]
JsonObject: TypeAlias = dict[str, JsonValue]


class ProtocolVersionWire(TypedDict):
    major: int
    minor: int


class DesktopStatusWire(TypedDict, total=False):
    id: Required[str]
    generation: Required[str | None]
    state: Required[str]
    reason_code: str | None


class StatusResponseWire(TypedDict, total=False):
    server_version: Required[str]
    protocol_min: Required[ProtocolVersionWire]
    protocol_max: Required[ProtocolVersionWire]
    server_time: Required[str]
    desktop: Required[DesktopStatusWire]
    capabilities: Required[JsonObject]


class CommandResultWire(TypedDict, total=False):
    command_id: Required[str]
    lifecycle: Required[str]
    effect_stage: Required[str]
    accepted_at: Required[str]
    warnings: Required[list[JsonValue]]
    outcome: JsonValue
    problem: JsonValue


class LeaseStateWire(TypedDict, total=False):
    desktop_id: Required[str]
    desktop_generation: Required[str]
    state: Required[str]
    lease_id: str | None
    expires_at: str | None


class EventWire(TypedDict, total=False):
    desktop_id: Required[str]
    desktop_generation: Required[str]
    sequence: Required[CanonicalUInt64]
    topic: Required[str]
    payload: Required[JsonValue]


class EventMessageWire(TypedDict, total=False):
    type: Required[str]
    request_id: Required[str]
    event: Required[EventWire]
