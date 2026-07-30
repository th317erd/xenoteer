// SPDX-License-Identifier: Apache-2.0

import { XenoteerError } from "./errors.js";
import type { SdkErrorCode } from "./errors.js";
import type {
  ReconnectPolicy,
  WebSocketFactory,
} from "./events.js";
import type { ProtocolRange } from "./protocol.generated.js";

export type TokenProvider = () => string | Promise<string>;
export type TokenSource = string | TokenProvider;

export type SafeLogOperation =
  | "http.request"
  | "artifact.upload"
  | "artifact.download"
  | "artifact.delete"
  | "websocket.handshake";

export type SafeLogOutcome = "started" | "succeeded" | "failed";

export type SafeLogRoute =
  | "/v1/status"
  | "/v1/ws"
  | "/v1/artifacts"
  | "/v1/artifacts/:artifact_id"
  | "/v1/desktops/:desktop_id/commands"
  | "/v1/desktops/:desktop_id/commands/:command_id"
  | "/v1/desktops/:desktop_id/lease"
  | "/v1/desktops/:desktop_id/lease/:lease_id/renew"
  | "/v1/desktops/:desktop_id/windows"
  | "/v1/desktops/:desktop_id/windows/query"
  | "/v1/desktops/:desktop_id/windows/resolve"
  | "/v1/desktops/:desktop_id/windows/:window_reference"
  | "/v1/desktops/:desktop_id/windows/wait"
  | "/v1/desktops/:desktop_id/accessibility/elements/list"
  | "/v1/desktops/:desktop_id/accessibility/elements/query"
  | "/v1/desktops/:desktop_id/accessibility/elements/resolve"
  | "/v1/desktops/:desktop_id/accessibility/elements/snapshot"
  | "/v1/desktops/:desktop_id/accessibility/elements/wait"
  | "/v1/desktops/:desktop_id/clipboard/read"
  | "/v1/desktops/:desktop_id/screenshots"
  | "/v1/desktops/:desktop_id/viewer-tickets"
  | "unknown";

export interface SafeLogEvent {
  readonly operation: SafeLogOperation;
  readonly outcome: SafeLogOutcome;
  readonly attempt?: number;
  readonly method?: "GET" | "POST" | "DELETE";
  readonly route?: SafeLogRoute;
  readonly status?: number;
  readonly requestBytes?: number;
  readonly responseBytes?: number;
  readonly errorCode?: SdkErrorCode;
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
  /** Header-capable WebSocket adapter retained for initial and reconnect attempts. */
  readonly webSocketFactory?: WebSocketFactory;
  readonly reconnect?: ReconnectPolicy;
  readonly webSocketQueueCapacity?: number;
  readonly webSocketMaxMessageBytes?: number;
  readonly webSocketHandshakeTimeoutMs?: number;
  readonly webSocketAcknowledgmentTimeoutMs?: number;
  readonly webSocketHeartbeatGraceMs?: number;
  readonly webSocketCloseTimeoutMs?: number;
}

function validateInteger(
  value: number,
  minimum: number,
  maximum: number,
  label: string,
): number {
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new XenoteerError("invalid_request", `${label} is outside its supported range`);
  }
  return value;
}

function validateDescriptor(value: unknown, label: string): string {
  if (
    typeof value !== "string"
    || new TextEncoder().encode(value).byteLength < 1
    || new TextEncoder().encode(value).byteLength > 128
    || /[\u0000-\u001f\u007f-\u009f]/u.test(value)
  ) {
    throw new XenoteerError(
      "invalid_request",
      `${label} must be 1..128 UTF-8 bytes without control characters`,
    );
  }
  return value;
}

function snapshotProtocolRange(
  value: ProtocolRange | undefined,
): Readonly<ProtocolRange> | undefined {
  if (value === undefined) return undefined;
  if (
    typeof value !== "object"
    || value === null
    || !Number.isInteger(value.major)
    || !Number.isInteger(value.minMinor)
    || !Number.isInteger(value.maxMinor)
    || value.major !== 1
    || value.minMinor < 0
    || value.minMinor > 65_535
    || value.maxMinor < 0
    || value.maxMinor > 65_535
    || value.minMinor > value.maxMinor
  ) {
    throw new XenoteerError(
      "invalid_request",
      "protocolRange must be a valid version-one minor range",
    );
  }
  return Object.freeze({
    major: value.major,
    minMinor: value.minMinor,
    maxMinor: value.maxMinor,
  });
}

function snapshotReconnect(
  reconnect: ReconnectPolicy | undefined,
): Readonly<ReconnectPolicy> | undefined {
  if (reconnect === undefined) return undefined;
  const maxAttempts = validateInteger(
    reconnect.maxAttempts ?? 5,
    0,
    100,
    "reconnect maxAttempts",
  );
  const initialDelayMs = validateInteger(
    reconnect.initialDelayMs ?? 100,
    1,
    60_000,
    "reconnect initial delay",
  );
  const maxDelayMs = validateInteger(
    reconnect.maxDelayMs ?? 5_000,
    1,
    120_000,
    "reconnect maximum delay",
  );
  if (initialDelayMs > maxDelayMs) {
    throw new XenoteerError("invalid_request", "reconnect delays are reversed");
  }
  return Object.freeze({ maxAttempts, initialDelayMs, maxDelayMs });
}

/**
 * Takes the one retained connection-policy snapshot before any network I/O.
 * Callback references are borrowed for the client lifetime; mutable numeric
 * policy and protocol objects are copied and frozen.
 *
 * @internal
 */
export function snapshotClientOptions(
  options: XenoteerClientOptions,
): Readonly<XenoteerClientOptions> {
  const protocolRange = snapshotProtocolRange(options.protocolRange);
  const clientName = validateDescriptor(
    options.clientName ?? "@xenoteer/sdk",
    "clientName",
  );
  const clientVersion = validateDescriptor(
    options.clientVersion ?? "0.1.0",
    "clientVersion",
  );
  const reconnect = snapshotReconnect(options.reconnect);
  const snapshot = {
    baseUrl: options.baseUrl,
    token: options.token,
    ...(options.requestTimeoutMs === undefined
      ? {}
      : { requestTimeoutMs: options.requestTimeoutMs }),
    ...(options.maxResponseBytes === undefined
      ? {}
      : { maxResponseBytes: options.maxResponseBytes }),
    ...(options.maxArtifactBytes === undefined
      ? {}
      : { maxArtifactBytes: options.maxArtifactBytes }),
    ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
    ...(options.log === undefined ? {} : { log: options.log }),
    clientName,
    clientVersion,
    ...(protocolRange === undefined ? {} : { protocolRange }),
    ...(options.webSocketFactory === undefined
      ? {}
      : { webSocketFactory: options.webSocketFactory }),
    ...(reconnect === undefined ? {} : { reconnect }),
    webSocketQueueCapacity: validateInteger(
      options.webSocketQueueCapacity ?? 256,
      1,
      4096,
      "WebSocket queue capacity",
    ),
    webSocketMaxMessageBytes: validateInteger(
      options.webSocketMaxMessageBytes ?? 1_048_576,
      1024,
      16 * 1_048_576,
      "WebSocket message limit",
    ),
    webSocketHandshakeTimeoutMs: validateInteger(
      options.webSocketHandshakeTimeoutMs ?? 10_000,
      1,
      60_000,
      "WebSocket handshake timeout",
    ),
    webSocketAcknowledgmentTimeoutMs: validateInteger(
      options.webSocketAcknowledgmentTimeoutMs ?? 10_000,
      1,
      60_000,
      "WebSocket acknowledgment timeout",
    ),
    webSocketHeartbeatGraceMs: validateInteger(
      options.webSocketHeartbeatGraceMs ?? 5_000,
      1,
      120_000,
      "WebSocket heartbeat grace",
    ),
    webSocketCloseTimeoutMs: validateInteger(
      options.webSocketCloseTimeoutMs ?? 2_000,
      1,
      30_000,
      "WebSocket close timeout",
    ),
  } satisfies XenoteerClientOptions;
  return Object.freeze(snapshot);
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
    webSocketFactory: options.webSocketFactory === undefined
      ? undefined
      : "<configured>",
    reconnect: options.reconnect === undefined
      ? undefined
      : {
          maxAttempts: options.reconnect.maxAttempts ?? 5,
          initialDelayMs: options.reconnect.initialDelayMs ?? 100,
          maxDelayMs: options.reconnect.maxDelayMs ?? 5_000,
        },
    webSocketQueueCapacity: options.webSocketQueueCapacity ?? 256,
    webSocketMaxMessageBytes: options.webSocketMaxMessageBytes ?? 1_048_576,
    webSocketHandshakeTimeoutMs: options.webSocketHandshakeTimeoutMs ?? 10_000,
    webSocketAcknowledgmentTimeoutMs:
      options.webSocketAcknowledgmentTimeoutMs ?? 10_000,
    webSocketHeartbeatGraceMs: options.webSocketHeartbeatGraceMs ?? 5_000,
    webSocketCloseTimeoutMs: options.webSocketCloseTimeoutMs ?? 2_000,
    protocolRange: options.protocolRange ?? {
      major: 1,
      minMinor: 0,
      maxMinor: 0,
    },
  });
}
