# SPDX-License-Identifier: Apache-2.0
"""Precision-safe helpers for public v1 wire integers."""

from __future__ import annotations

import re
import copy
from collections.abc import Mapping
from typing import Final

from .protocol_generated import CanonicalUInt64


UINT64_MAX: Final = (1 << 64) - 1
_CANONICAL_UINT64 = re.compile(r"(?:0|[1-9][0-9]{0,19})\Z")
_CANONICAL_NONZERO_UINT64 = re.compile(r"[1-9][0-9]{0,19}\Z")
_PRECISION_SENSITIVE_FIELDS: Final = frozenset(
    {
        "after_revision",
        "app_instance_generation",
        "atspi_generation",
        "cache_sequence",
        "dropped_through_sequence",
        "evaluated_revision",
        "event_sequence",
        "first_observed_revision",
        "latest_sequence",
        "last_observed_revision",
        "model_revision",
        "observed_generation",
        "previous_revision",
        "proc_start_ticks",
        "revision",
        "sequence",
        "snapshot_revision",
        "through_sequence",
    }
)
_NONZERO_FIELDS: Final = _PRECISION_SENSITIVE_FIELDS


def decode_uint64(wire: object, *, allow_zero: bool = True) -> int:
    """Decode exact canonical decimal wire text without passing through float."""

    pattern = _CANONICAL_UINT64 if allow_zero else _CANONICAL_NONZERO_UINT64
    if not isinstance(wire, str) or pattern.fullmatch(wire) is None:
        raise TypeError("invalid canonical uint64 string")
    value = int(wire, 10)
    if value > UINT64_MAX:
        raise TypeError("invalid canonical uint64 string")
    return value


def encode_uint64(value: int, *, allow_zero: bool = True) -> CanonicalUInt64:
    """Encode an integer as exact canonical decimal wire text."""

    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > UINT64_MAX
        or (not allow_zero and value == 0)
    ):
        raise ValueError("value is outside canonical uint64 range")
    return CanonicalUInt64(str(value))


def as_uint64_string(wire: object, *, allow_zero: bool = True) -> CanonicalUInt64:
    """Validate and narrow already encoded wire text while retaining its digits."""

    decode_uint64(wire, allow_zero=allow_zero)
    assert isinstance(wire, str)
    return CanonicalUInt64(wire)


def canonicalize_uint64_fields(value: object) -> object:
    """Copy an outbound tree and stringify every known precision-sensitive u64."""

    if isinstance(value, Mapping):
        result: dict[str, object] = {}
        for key, child in value.items():
            if not isinstance(key, str):
                raise TypeError("JSON object keys must be strings")
            if key in _PRECISION_SENSITIVE_FIELDS and child is not None:
                allow_zero = key not in _NONZERO_FIELDS
                if isinstance(child, str):
                    result[key] = as_uint64_string(child, allow_zero=allow_zero)
                elif isinstance(child, int) and not isinstance(child, bool):
                    result[key] = encode_uint64(child, allow_zero=allow_zero)
                else:
                    raise TypeError(f"{key} must be a canonical uint64 string or integer")
            else:
                result[key] = canonicalize_uint64_fields(child)
        return result
    if isinstance(value, (list, tuple)):
        return [canonicalize_uint64_fields(child) for child in value]
    return copy.deepcopy(value)


def validate_uint64_fields(value: object) -> None:
    """Reject non-string precision-sensitive counters in an inbound tree."""

    if isinstance(value, Mapping):
        for key, child in value.items():
            if key in _PRECISION_SENSITIVE_FIELDS and child is not None:
                as_uint64_string(child, allow_zero=key not in _NONZERO_FIELDS)
            else:
                validate_uint64_fields(child)
    elif isinstance(value, list):
        for child in value:
            validate_uint64_fields(child)
