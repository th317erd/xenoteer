// SPDX-License-Identifier: Apache-2.0

import { Desktop } from "./desktop.js";
import { validateStatusResponse } from "./compatibility.js";
import {
  EventSession,
  type EventSessionOptions,
  type WebSocketFactory,
} from "./events.js";
import { XenoteerError } from "./errors.js";
import {
  redactedClientOptions,
  snapshotClientOptions,
  type XenoteerClientOptions,
} from "./options.js";
import type {
  JsonObject,
  ProtocolRange,
  ProtocolVersion,
  StatusResponse,
} from "./protocol.generated.js";
import { HttpTransport } from "./transport.js";

const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");
const DEFAULT_RANGE: ProtocolRange = {
  major: 1,
  minMinor: 0,
  maxMinor: 0,
};
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isVersion(value: unknown): value is ProtocolVersion {
  return isObject(value)
    && Number.isInteger(value["major"])
    && Number.isInteger(value["minor"])
    && (value["major"] as number) >= 0
    && (value["minor"] as number) >= 0
    && (value["major"] as number) <= 65_535
    && (value["minor"] as number) <= 65_535;
}

function validateRange(range: ProtocolRange): void {
  if (
    !isObject(range)
    || !Number.isInteger(range.major)
    || !Number.isInteger(range.minMinor)
    || !Number.isInteger(range.maxMinor)
    || range.major < 0
    || range.major > 65_535
    || range.minMinor < 0
    || range.minMinor > 65_535
    || range.maxMinor < 0
    || range.maxMinor > 65_535
  ) {
    throw new XenoteerError("invalid_request", "client protocol range is invalid");
  }
  if (range.minMinor > range.maxMinor) {
    throw new XenoteerError(
      "reversed_minor_range",
      "client protocol range is reversed",
    );
  }
}

/** Selects the highest minor in one shared major, or fails closed. */
export function negotiateProtocol(
  client: ProtocolRange,
  serverMin: ProtocolVersion,
  serverMax: ProtocolVersion,
): ProtocolVersion {
  validateRange(client);
  if (
    !isVersion(serverMin)
    || !isVersion(serverMax)
  ) {
    throw new XenoteerError("invalid_response", "server protocol range is invalid");
  }
  if (serverMin.minor > serverMax.minor) {
    throw new XenoteerError(
      "reversed_minor_range",
      "server protocol range is reversed",
    );
  }
  if (
    serverMin.major !== serverMax.major
    || client.major !== serverMin.major
  ) {
    throw new XenoteerError(
      "unsupported_major",
      "client and server protocol majors differ",
    );
  }
  const minimum = Math.max(client.minMinor, serverMin.minor);
  const maximum = Math.min(client.maxMinor, serverMax.minor);
  if (minimum > maximum) {
    throw new XenoteerError(
      "no_shared_minor",
      "client and server share no protocol minor",
    );
  }
  return Object.freeze({ major: client.major, minor: maximum });
}

/** Enforces the exact negotiated version fence on every post-handshake request. */
export function admitRequestVersion(
  negotiated: ProtocolVersion,
  request: ProtocolVersion,
): void {
  if (!isVersion(negotiated) || !isVersion(request)) {
    throw new XenoteerError("invalid_request", "protocol version is invalid");
  }
  if (negotiated.major !== request.major || negotiated.minor !== request.minor) {
    throw new XenoteerError(
      "unsupported_version",
      "request version differs from negotiation",
    );
  }
}

export interface OpenEventSessionOptions extends EventSessionOptions {
  readonly resume?: {
    readonly desktopId: string;
    readonly desktopGeneration: string;
    readonly eventSequence: import("./protocol.generated.js").CanonicalUInt64;
  } | null;
}

/** Connected, negotiated SDK root. Construction never acquires control implicitly. */
export class XenoteerClient implements AsyncDisposable {
  readonly #transport: HttpTransport;
  #status: StatusResponse;
  readonly #protocol: ProtocolVersion;
  readonly #clientName: string;
  readonly #clientVersion: string;
  readonly #webSocketFactory: WebSocketFactory | undefined;
  readonly #eventPolicy: Readonly<EventSessionOptions>;
  readonly #safeOptions: Readonly<Record<string, unknown>>;

  private constructor(
    transport: HttpTransport,
    status: StatusResponse,
    protocol: ProtocolVersion,
    options: XenoteerClientOptions,
  ) {
    this.#transport = transport;
    this.#status = status;
    this.#protocol = protocol;
    this.#clientName = options.clientName ?? "@xenoteer/sdk";
    this.#clientVersion = options.clientVersion ?? "0.1.0";
    this.#webSocketFactory = options.webSocketFactory;
    this.#eventPolicy = Object.freeze({
      capacity: options.webSocketQueueCapacity ?? 256,
      maxMessageBytes: options.webSocketMaxMessageBytes ?? 1_048_576,
      handshakeTimeoutMs: options.webSocketHandshakeTimeoutMs ?? 10_000,
      acknowledgmentTimeoutMs:
        options.webSocketAcknowledgmentTimeoutMs ?? 10_000,
      heartbeatGraceMs: options.webSocketHeartbeatGraceMs ?? 5_000,
      closeTimeoutMs: options.webSocketCloseTimeoutMs ?? 2_000,
      ...(options.reconnect === undefined
        ? {}
        : {
            reconnect: Object.freeze({
              maxAttempts: options.reconnect.maxAttempts ?? 5,
              initialDelayMs: options.reconnect.initialDelayMs ?? 100,
              maxDelayMs: options.reconnect.maxDelayMs ?? 5_000,
            }),
          }),
    });
    this.#safeOptions = redactedClientOptions(options);
  }

  static async connect(options: XenoteerClientOptions): Promise<XenoteerClient> {
    const retained = snapshotClientOptions(options);
    const transport = new HttpTransport(retained);
    const status = validateStatusResponse(
      await transport.request<object>("GET", "/v1/status"),
    );
    const protocol = negotiateProtocol(
      retained.protocolRange ?? DEFAULT_RANGE,
      status.protocol_min,
      status.protocol_max,
    );
    const generation = status.desktop.generation;
    if (typeof generation === "string") {
      transport.state.bindDesktop(status.desktop.id, generation);
    }
    return new XenoteerClient(transport, status, protocol, retained);
  }

  get status(): StatusResponse {
    return this.#status;
  }

  get negotiatedProtocol(): ProtocolVersion {
    return this.#protocol;
  }

  desktop(): Desktop {
    this.#ensureOpen();
    const generation = this.#status.desktop.generation;
    if (generation === undefined || generation === null) {
      throw new XenoteerError("desktop_unavailable", "desktop session is not currently available");
    }
    this.#transport.state.assertGeneration(
      this.#status.desktop.id,
      generation,
      "reference",
    );
    return new Desktop(
      this.#transport,
      this.#status.desktop.id,
      generation,
      this.#protocol,
    );
  }

  /** Refreshes status after an explicit resynchronization boundary. */
  async refreshStatus(): Promise<StatusResponse> {
    this.#ensureOpen();
    const status = validateStatusResponse(
      await this.#transport.request<object>("GET", "/v1/status"),
    );
    negotiateProtocol(
      {
        major: this.#protocol.major,
        minMinor: this.#protocol.minor,
        maxMinor: this.#protocol.minor,
      },
      status.protocol_min,
      status.protocol_max,
    );
    const generation = status.desktop.generation;
    if (typeof generation === "string") {
      this.#transport.state.observeDesktop(status.desktop.id, generation);
    }
    this.#status = status;
    return status;
  }

  /**
   * Opens the authenticated v1 WebSocket with the root retained factory.
   */
  async openEventSession(
    options?: OpenEventSessionOptions,
  ): Promise<EventSession>;
  /** Compatibility overload. @deprecated Retain `webSocketFactory` at `connect()`. */
  async openEventSession(
    factory: WebSocketFactory,
    options?: OpenEventSessionOptions,
  ): Promise<EventSession>;
  async openEventSession(
    factoryOrOptions?: WebSocketFactory | OpenEventSessionOptions,
    compatibilityOptions: OpenEventSessionOptions = {},
  ): Promise<EventSession> {
    this.#ensureOpen();
    const compatibilityFactory = typeof factoryOrOptions === "function"
      ? factoryOrOptions
      : undefined;
    const factory = compatibilityFactory ?? this.#webSocketFactory;
    if (factory === undefined) {
      throw new XenoteerError(
        "invalid_request",
        "openEventSession requires an explicit header-capable WebSocket factory",
      );
    }
    const options: OpenEventSessionOptions =
      typeof factoryOrOptions === "function"
        ? compatibilityOptions
        : factoryOrOptions ?? {};
    const protocol = this.#protocol;
    const requestId = globalThis.crypto.randomUUID();
    const hello = {
      type: "client.hello",
      request_id: requestId,
      protocol: {
        major: protocol.major,
        min_minor: protocol.minor,
        max_minor: protocol.minor,
      },
      client: {
        name: this.#clientName,
        version: this.#clientVersion,
      },
      resume: options.resume === undefined || options.resume === null
        ? null
        : {
            desktop_id: options.resume.desktopId,
            desktop_generation: options.resume.desktopGeneration,
            event_sequence: options.resume.eventSequence,
          },
    } as JsonObject;
    const url = new URL("/v1/ws", this.#transport.baseUrl);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    return await EventSession.connect(
      factory,
      async () => ({
        url: url.toString(),
        authorization: await this.#transport.authorizationHeader(),
      }),
      hello,
      this.#transport.state,
      {
        ...this.#eventPolicy,
        ...options,
        ...((options.reconnect ?? this.#eventPolicy.reconnect) === undefined
          ? {}
          : { reconnect: options.reconnect ?? this.#eventPolicy.reconnect }),
        safeLog: (event) => this.#transport.safeLog(event),
        refreshAuthoritative: async () => {
          const status = await this.refreshStatus();
          const generation = status.desktop.generation;
          if (generation === undefined || generation === null) {
            return Object.freeze({ status, windows: null });
          }
          const desktop = this.desktop();
          return Object.freeze({
            status,
            windows: await desktop.windows.list(),
          });
        },
      },
    );
  }

  async close(): Promise<void> {
    await this.#transport.close();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  toString(): string {
    return `XenoteerClient(${JSON.stringify(this.#safeOptions)})`;
  }

  [inspectSymbol](): string {
    return this.toString();
  }

  #ensureOpen(): void {
    this.#transport.state.assertOpen();
  }
}
