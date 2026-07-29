// SPDX-License-Identifier: Apache-2.0

import { XenoteerError } from "./errors.js";
import type { ProtocolRange } from "./protocol.generated.js";

export type TokenProvider = () => string | Promise<string>;
export type TokenSource = string | TokenProvider;

export interface SafeLogEvent {
  readonly operation: string;
  readonly outcome: "started" | "succeeded" | "failed";
  readonly method?: string;
  readonly path?: string;
  readonly status?: number;
  readonly requestBytes?: number;
  readonly responseBytes?: number;
  readonly errorCode?: string;
}

export type SafeLogHook = (event: Readonly<SafeLogEvent>) => void;

const TOKEN68 = /^[A-Za-z0-9._~+/-]+={0,}$/u;
const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");

/** An opaque bearer credential whose string, JSON, and inspect forms are redacted. */
export class BearerToken {
  readonly #value: string;

  constructor(value: string) {
    if (
      value.length < 32
      || value.length > 1024
      || !TOKEN68.test(value)
      || /=[^=]/u.test(value)
    ) {
      throw new XenoteerError("invalid_token", "invalid Xenoteer bearer token");
    }
    this.#value = value;
  }

  authorizationHeader(): string {
    return `Bearer ${this.#value}`;
  }

  toString(): string {
    return "BearerToken(<redacted>)";
  }

  toJSON(): string {
    return "<redacted>";
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}

export interface XenoteerClientOptions {
  readonly baseUrl: string;
  readonly token: TokenSource;
  readonly requestTimeoutMs?: number;
  readonly maxResponseBytes?: number;
  readonly maxArtifactBytes?: number;
  readonly fetch?: typeof fetch;
  /**
   * Receives metadata-only diagnostics. Request/response bodies, credentials,
   * lease IDs, artifact IDs, viewer tickets, and server-provided prose are
   * deliberately never included.
   */
  readonly log?: SafeLogHook;
  readonly clientName?: string;
  readonly clientVersion?: string;
  readonly protocolRange?: ProtocolRange;
}

/** Safe diagnostic projection. This intentionally cannot reveal the token source. */
export function redactedClientOptions(
  options: XenoteerClientOptions,
): Readonly<Record<string, unknown>> {
  return Object.freeze({
    baseUrl: options.baseUrl,
    token: "<redacted>",
    requestTimeoutMs: options.requestTimeoutMs ?? 35_000,
    maxResponseBytes: options.maxResponseBytes ?? 1_048_576,
    maxArtifactBytes: options.maxArtifactBytes ?? 33_554_432,
    clientName: options.clientName ?? "@xenoteer/sdk",
    clientVersion: options.clientVersion ?? "0.1.0",
    log: options.log === undefined ? undefined : "<configured>",
    protocolRange: options.protocolRange ?? {
      major: 1,
      minMinor: 0,
      maxMinor: 0,
    },
  });
}
