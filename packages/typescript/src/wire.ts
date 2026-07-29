// SPDX-License-Identifier: Apache-2.0

import type { CanonicalUInt64 } from "./protocol.generated.js";

const UINT64_MAX = 18_446_744_073_709_551_615n;
const CANONICAL_UINT64 = /^(?:0|[1-9][0-9]{0,19})$/u;
const CANONICAL_NONZERO_UINT64 = /^[1-9][0-9]{0,19}$/u;

/** Validates a precision-sensitive unsigned 64-bit decimal wire value. */
export function decodeUInt64(
  wire: unknown,
  options: { readonly allowZero?: boolean } = {},
): bigint {
  const pattern = options.allowZero === false
    ? CANONICAL_NONZERO_UINT64
    : CANONICAL_UINT64;
  if (typeof wire !== "string" || !pattern.test(wire)) {
    throw new TypeError("invalid canonical uint64 string");
  }
  const value = BigInt(wire);
  if (value > UINT64_MAX) {
    throw new TypeError("invalid canonical uint64 string");
  }
  return value;
}

/** Encodes a bigint without ever passing through an imprecise number. */
export function encodeUInt64(
  value: bigint,
  options: { readonly allowZero?: boolean } = {},
): CanonicalUInt64 {
  if (
    value < 0n
    || value > UINT64_MAX
    || (options.allowZero === false && value === 0n)
  ) {
    throw new RangeError("value is outside canonical uint64 range");
  }
  return value.toString(10) as CanonicalUInt64;
}

/** Narrows already validated wire text while preserving its exact digits. */
export function asCanonicalUInt64(
  wire: unknown,
  options: { readonly allowZero?: boolean } = {},
): CanonicalUInt64 {
  decodeUInt64(wire, options);
  return wire as CanonicalUInt64;
}
