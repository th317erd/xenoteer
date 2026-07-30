// SPDX-License-Identifier: Apache-2.0

import { XenoteerError } from "./errors.js";
import type {
  SafeLogEvent,
  SafeLogHook,
} from "./options.js";
import type {
  CanonicalUInt64,
  EventMessageWire,
  EventWire,
  JsonObject,
  JsonValue,
  ProtocolVersion,
} from "./protocol.generated.js";
import type { ClientConnectionState } from "./transport.js";
import { asCanonicalUInt64, decodeUInt64 } from "./wire.js";

const KNOWN_TOPICS = new Set([
  "command.lifecycle",
  "action.lifecycle",
  "process.exited",
  "accessibility.element_created",
  "accessibility.element_changed",
  "accessibility.element_removed",
  "accessibility.resync_required",
]);
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;
const TOPIC = /^[a-z0-9]+(?:[._-][a-z0-9]+)*$/u;
const DESKTOP_STATES = new Set([
  "booting",
  "probing",
  "ready",
  "degraded",
  "draining",
  "stopped",
  "failed",
]);
const EVENT_RESYNC_REASONS = new Set([
  "generation_changed",
  "history_lost",
  "sequence_ahead",
  "subscriber_lag",
  "outbound_backpressure",
]);
const MAX_U32 = 0xffff_ffff;
const RESERVED_EVENT_CAPACITY = 4;

class TerminalHandshakeCloseError extends XenoteerError {
  constructor(code: number, opened: boolean) {
    super(
      "transport",
      opened
        ? `WebSocket closed with terminal code ${code} before welcome`
        : `WebSocket transport failed with terminal close code ${code}`,
    );
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function deepFreeze<T>(value: T): T {
  if (typeof value === "object" && value !== null && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const child of Object.values(value)) deepFreeze(child);
  }
  return value;
}

function validateUuid(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !UUID.test(value) || /^0{8}-0{4}-0{4}-0{4}-0{12}$/u.test(value)) {
    throw new XenoteerError("invalid_response", `WebSocket ${label} is invalid`);
  }
}

function utf8Length(text: string, stopAfter: number): number {
  let length = 0;
  for (const scalar of text) {
    const point = scalar.codePointAt(0) ?? 0;
    length += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
    if (length > stopAfter) return length;
  }
  return length;
}

function validTopic(topic: string): boolean {
  return TOPIC.test(topic)
    && utf8Length(topic, 128) >= 1
    && utf8Length(topic, 128) <= 128;
}

function validBoundedText(value: unknown, maximum: number): value is string {
  return typeof value === "string"
    && utf8Length(value, maximum) >= 1
    && utf8Length(value, maximum) <= maximum
    && !/[\u0000-\u001f\u007f]/u.test(value);
}

function validUnsignedCapacity(value: unknown, allowZero = false): value is number {
  return Number.isSafeInteger(value)
    && (allowZero ? (value as number) >= 0 : (value as number) >= 1)
    && (value as number) <= MAX_U32;
}

async function boundedText(data: unknown, limit: number): Promise<string> {
  if (typeof data === "string") {
    if (utf8Length(data, limit) > limit) {
      throw new XenoteerError("response_too_large", `WebSocket message exceeds ${limit} bytes`);
    }
    return data;
  }
  if (data instanceof ArrayBuffer) {
    if (data.byteLength > limit) {
      throw new XenoteerError("response_too_large", `WebSocket message exceeds ${limit} bytes`);
    }
    return new TextDecoder("utf-8", { fatal: true }).decode(data);
  }
  if (ArrayBuffer.isView(data)) {
    if (data.byteLength > limit) {
      throw new XenoteerError("response_too_large", `WebSocket message exceeds ${limit} bytes`);
    }
    return new TextDecoder("utf-8", { fatal: true }).decode(
      new Uint8Array(data.buffer, data.byteOffset, data.byteLength),
    );
  }
  if (typeof Blob !== "undefined" && data instanceof Blob) {
    if (data.size > limit) {
      throw new XenoteerError("response_too_large", `WebSocket message exceeds ${limit} bytes`);
    }
    return new TextDecoder("utf-8", { fatal: true }).decode(await data.arrayBuffer());
  }
  throw new XenoteerError("invalid_response", "WebSocket message has an unsupported representation");
}

export interface KnownEvent {
  readonly kind: "known";
  readonly topic: string;
  readonly sequence: bigint;
  readonly payload: JsonValue;
  readonly raw: EventMessageWire;
}

export interface UnknownEvent {
  readonly kind: "unknown";
  readonly topic: string;
  readonly sequence: bigint;
  readonly payload: JsonValue;
  readonly raw: EventMessageWire;
}

export interface ResyncRequiredEvent {
  readonly kind: "resync_required";
  readonly reason: string;
  readonly droppedThrough?: bigint;
  readonly latestSequence?: bigint;
  readonly raw: Readonly<Record<string, unknown>>;
}

export interface ReplayCompleteEvent {
  readonly kind: "replay_complete";
  readonly throughSequence: bigint;
  readonly raw: Readonly<Record<string, unknown>>;
}

export interface UnknownServerMessage {
  readonly kind: "unknown_message";
  readonly raw: Readonly<Record<string, unknown>>;
}

export type XenoteerEvent =
  | KnownEvent
  | UnknownEvent
  | ReplayCompleteEvent
  | ResyncRequiredEvent
  | UnknownServerMessage;

/** Decodes a server event without coercing sequence digits or discarding future topics. */
export function decodeEventMessage(value: unknown): KnownEvent | UnknownEvent {
  if (!isObject(value) || value["type"] !== "event" || typeof value["request_id"] !== "string") {
    throw new XenoteerError("invalid_response", "invalid Xenoteer event envelope");
  }
  validateUuid(value["request_id"], "event request ID");
  const event = value["event"];
  if (
    !isObject(event)
    || typeof event["desktop_id"] !== "string"
    || typeof event["desktop_generation"] !== "string"
    || typeof event["topic"] !== "string"
    || !validTopic(event["topic"])
    || !Object.hasOwn(event, "payload")
  ) {
    throw new XenoteerError("invalid_response", "invalid Xenoteer event");
  }
  validateUuid(event["desktop_id"], "event desktop ID");
  validateUuid(event["desktop_generation"], "event desktop generation");
  const sequence = decodeUInt64(event["sequence"], { allowZero: false });
  const cloned = structuredClone(value) as Record<string, unknown>;
  const clonedEvent = cloned["event"] as Record<string, unknown>;
  const normalized = {
    ...cloned,
    event: {
      ...clonedEvent,
      sequence: asCanonicalUInt64(clonedEvent["sequence"], { allowZero: false }),
    } as EventWire,
  } as EventMessageWire;
  const raw = deepFreeze(normalized);
  return {
    kind: KNOWN_TOPICS.has(raw.event.topic) ? "known" : "unknown",
    topic: raw.event.topic,
    sequence,
    payload: raw.event.payload,
    raw,
  };
}

export interface WebSocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: "open", listener: () => void, options?: { once?: boolean }): void;
  addEventListener(type: "message", listener: (event: { readonly data: unknown }) => void): void;
  addEventListener(type: "error", listener: () => void, options?: { once?: boolean }): void;
  addEventListener(
    type: "close",
    listener: (event?: { readonly code?: number; readonly reason?: string }) => void,
    options?: { once?: boolean },
  ): void;
}

export interface AuthenticatedWebSocketOptions {
  readonly url: string;
  /** Sensitive. Implementations must set this as the HTTP Authorization header. */
  readonly authorization: string;
  /** Adapter-enforced inbound frame/message ceiling for this attempt. */
  readonly maxMessageBytes: number;
  /** Absolute Unix-epoch millisecond deadline for completing the upgrade. */
  readonly handshakeDeadlineMs: number;
}

export type WebSocketFactory = (
  options: AuthenticatedWebSocketOptions,
) => WebSocketLike;

export interface ReconnectPolicy {
  readonly maxAttempts?: number;
  readonly initialDelayMs?: number;
  readonly maxDelayMs?: number;
}

export interface EventSessionOptions {
  readonly capacity?: number;
  readonly maxMessageBytes?: number;
  readonly handshakeTimeoutMs?: number;
  readonly acknowledgmentTimeoutMs?: number;
  readonly heartbeatGraceMs?: number;
  readonly closeTimeoutMs?: number;
  readonly reconnect?: ReconnectPolicy;
  /** @internal Populated by XenoteerClient for explicit resynchronization. */
  readonly refreshAuthoritative?: () => Promise<Readonly<Record<string, unknown>>>;
  /** @internal Populated by XenoteerClient with its fail-closed safe logger. */
  readonly safeLog?: SafeLogHook;
}

export interface EventSubscription {
  readonly topics: readonly string[];
  readonly sinceSequence: CanonicalUInt64 | null;
}

export interface EventSessionTerminalReason {
  readonly code:
    | "client_closed"
    | "normal"
    | "transport"
    | "handshake_timeout"
    | "heartbeat_timeout"
    | "invalid_message"
    | "message_too_large"
    | "backpressure"
    | "resync_required"
    | "generation_changed"
    | "server_draining";
  readonly detail: string;
}

interface Welcome {
  readonly protocol: ProtocolVersion;
  readonly desktopId: string;
  readonly desktopGeneration: string;
  readonly heartbeatMs: number;
  readonly maxMessageBytes: number;
  readonly resumeStatus: "not_requested" | "replayed" | "resync_required";
  readonly raw: Readonly<Record<string, unknown>>;
}

interface WebSocketCredentials {
  readonly url: string;
  readonly authorization: string;
}

type SocketOptionsProvider =
  () => Promise<WebSocketCredentials>;

interface PendingResponse {
  readonly expectedType: string;
  readonly expectedNonce?: string;
  readonly resolve: (value: Readonly<Record<string, unknown>>) => void;
  readonly reject: (error: XenoteerError) => void;
  readonly timer: ReturnType<typeof setTimeout>;
}

function safeDelay(value: number | undefined, fallback: number, maximum: number, label: string): number {
  const selected = value ?? fallback;
  if (!Number.isSafeInteger(selected) || selected < 1 || selected > maximum) {
    throw new XenoteerError("invalid_request", `${label} is outside its supported range`);
  }
  return selected;
}

function validateTopics(topics: readonly string[]): readonly string[] {
  if (topics.length > 32) {
    throw new XenoteerError("invalid_request", "event topics must contain at most 32 values");
  }
  const result = topics.map((topic) => {
    if (
      typeof topic !== "string"
      || !validTopic(topic)
    ) {
      throw new XenoteerError("invalid_request", "event topic is invalid");
    }
    return topic;
  });
  if (new Set(result).size !== result.length) {
    throw new XenoteerError("invalid_request", "event topics must be unique");
  }
  return Object.freeze([...result]);
}

/**
 * Bounded authenticated event session with timed welcome, correlated
 * subscriptions, heartbeat supervision, and same-generation replay reconnect.
 */
export class EventSession implements AsyncDisposable, AsyncIterable<XenoteerEvent> {
  readonly #factory: WebSocketFactory;
  readonly #socketOptions: SocketOptionsProvider;
  readonly #baseHello: JsonObject;
  readonly #state: ClientConnectionState;
  readonly #capacity: number;
  readonly #configuredMaxMessageBytes: number;
  readonly #handshakeTimeoutMs: number;
  readonly #ackTimeoutMs: number;
  readonly #heartbeatGraceMs: number;
  readonly #closeTimeoutMs: number;
  readonly #maxReconnectAttempts: number;
  readonly #initialReconnectDelayMs: number;
  readonly #maxReconnectDelayMs: number;
  readonly #queue: XenoteerEvent[] = [];
  readonly #waiters: Array<(value: IteratorResult<XenoteerEvent>) => void> = [];
  readonly #pending = new Map<string, PendingResponse>();
  readonly #failedCandidatesClosed = new WeakSet<object>();
  readonly #cancellation = new AbortController();
  readonly #unregisterStateClose: () => void;
  readonly #safeLog: SafeLogHook | undefined;
  readonly #refreshAuthoritative:
    | (() => Promise<Readonly<Record<string, unknown>>>)
    | undefined;
  #socket: WebSocketLike | undefined;
  #welcome?: Welcome;
  #subscription: EventSubscription | undefined;
  #subscriptionRequestId: string | undefined;
  #lastSequence: bigint | undefined;
  #ordinaryQueued = 0;
  #reservedQueued = 0;
  #closed = false;
  #intentionalClose = false;
  #reconnecting = false;
  #heartbeatTimer?: ReturnType<typeof setInterval>;
  #terminal?: EventSessionTerminalReason;
  #handshakeAttempt = 0;

  private constructor(
    factory: WebSocketFactory,
    socketOptions: SocketOptionsProvider,
    hello: JsonObject,
    state: ClientConnectionState,
    options: EventSessionOptions,
  ) {
    this.#factory = factory;
    this.#socketOptions = socketOptions;
    this.#baseHello = deepFreeze(structuredClone(hello));
    this.#state = state;
    this.#capacity = safeDelay(options.capacity, 256, 4096, "event queue capacity");
    this.#configuredMaxMessageBytes = safeDelay(
      options.maxMessageBytes,
      1_048_576,
      16 * 1_048_576,
      "WebSocket message limit",
    );
    this.#handshakeTimeoutMs = safeDelay(
      options.handshakeTimeoutMs,
      10_000,
      60_000,
      "WebSocket handshake timeout",
    );
    this.#ackTimeoutMs = safeDelay(
      options.acknowledgmentTimeoutMs,
      10_000,
      60_000,
      "WebSocket acknowledgment timeout",
    );
    this.#heartbeatGraceMs = safeDelay(
      options.heartbeatGraceMs,
      5_000,
      120_000,
      "WebSocket heartbeat grace",
    );
    this.#closeTimeoutMs = safeDelay(
      options.closeTimeoutMs,
      2_000,
      30_000,
      "WebSocket close timeout",
    );
    this.#refreshAuthoritative = options.refreshAuthoritative;
    this.#maxReconnectAttempts = options.reconnect?.maxAttempts ?? 5;
    if (!Number.isSafeInteger(this.#maxReconnectAttempts) || this.#maxReconnectAttempts < 0 || this.#maxReconnectAttempts > 100) {
      throw new XenoteerError("invalid_request", "reconnect maxAttempts must be 0..100");
    }
    this.#initialReconnectDelayMs = safeDelay(
      options.reconnect?.initialDelayMs,
      100,
      60_000,
      "reconnect initial delay",
    );
    this.#maxReconnectDelayMs = safeDelay(
      options.reconnect?.maxDelayMs,
      5_000,
      120_000,
      "reconnect maximum delay",
    );
    if (this.#initialReconnectDelayMs > this.#maxReconnectDelayMs) {
      throw new XenoteerError("invalid_request", "reconnect delays are reversed");
    }
    this.#safeLog = options.safeLog;
    this.#unregisterStateClose = state.register(async () => {
      await this.#terminate("client_closed", "owning Xenoteer client closed", 1000);
    });
  }

  static async connect(
    factory: WebSocketFactory,
    socketOptions:
      | WebSocketCredentials
      | SocketOptionsProvider,
    hello: JsonObject,
    state: ClientConnectionState,
    options: EventSessionOptions = {},
  ): Promise<EventSession> {
    const provider = typeof socketOptions === "function"
      ? socketOptions
      : async () => socketOptions;
    const session = new EventSession(factory, provider, hello, state, options);
    try {
      await session.#establish(hello);
      return session;
    } catch (error) {
      await session.#terminate(
        error instanceof XenoteerError && error.code === "websocket_timeout"
          ? "handshake_timeout"
          : "transport",
        "WebSocket session establishment failed",
        1002,
      );
      throw error;
    }
  }

  get welcome(): Readonly<Record<string, unknown>> {
    this.#ensureOpen();
    const welcome = this.#welcome;
    if (welcome === undefined) {
      throw new XenoteerError("session_closed", "WebSocket welcome is unavailable");
    }
    return welcome.raw;
  }

  get terminalReason(): EventSessionTerminalReason | undefined {
    return this.#terminal;
  }

  get lastSequence(): bigint | undefined {
    return this.#lastSequence;
  }

  async subscribe(
    topics: readonly string[],
    sinceSequence: CanonicalUInt64 | null = null,
  ): Promise<EventSubscription> {
    this.#ensureOpen();
    const welcome = this.#welcome;
    if (welcome === undefined) {
      throw new XenoteerError("session_closed", "WebSocket welcome is unavailable");
    }
    const validatedTopics = validateTopics(topics);
    if (sinceSequence !== null) decodeUInt64(sinceSequence);
    const requestId = globalThis.crypto.randomUUID();
    const pending = this.#expect(requestId, "events.subscribed");
    this.#send({
      type: "events.subscribe",
      request_id: requestId,
      desktop_id: welcome.desktopId,
      desktop_generation: welcome.desktopGeneration,
      topics: validatedTopics,
      since_sequence: sinceSequence,
    });
    const response = await pending;
    if (!Array.isArray(response["topics"]) || response["topics"].some((value) => typeof value !== "string")) {
      throw new XenoteerError("invalid_response", "event subscription acknowledgment is invalid");
    }
    const acknowledged = response["topics"] as string[];
    if (
      acknowledged.length !== validatedTopics.length
      || acknowledged.some((topic, index) => topic !== validatedTopics[index])
    ) {
      throw new XenoteerError("invalid_response", "event subscription acknowledgment changed topics");
    }
    const subscription = Object.freeze({
      topics: validatedTopics,
      sinceSequence,
    });
    this.#subscription = subscription;
    this.#subscriptionRequestId = requestId;
    if (sinceSequence !== null) {
      const lowerBound = decodeUInt64(sinceSequence);
      if (
        this.#lastSequence === undefined
        || lowerBound > this.#lastSequence
      ) {
        this.#lastSequence = lowerBound;
      }
    }
    return subscription;
  }

  async unsubscribe(): Promise<void> {
    this.#ensureOpen();
    const welcome = this.#welcome;
    if (welcome === undefined) {
      throw new XenoteerError("session_closed", "WebSocket welcome is unavailable");
    }
    const requestId = globalThis.crypto.randomUUID();
    const pending = this.#expect(requestId, "events.unsubscribed");
    this.#send({
      type: "events.unsubscribe",
      request_id: requestId,
      desktop_id: welcome.desktopId,
      desktop_generation: welcome.desktopGeneration,
    });
    await pending;
    this.#subscription = undefined;
    this.#subscriptionRequestId = undefined;
  }

  async refreshAuthoritativeSnapshots(): Promise<Readonly<Record<string, unknown>>> {
    if (this.#refreshAuthoritative === undefined) {
      throw new XenoteerError(
        "invalid_request",
        "this session has no authoritative refresh helper",
      );
    }
    return await this.#refreshAuthoritative();
  }

  [Symbol.asyncIterator](): AsyncIterator<XenoteerEvent> {
    return {
      next: async (): Promise<IteratorResult<XenoteerEvent>> => {
        const queued = this.#dequeue();
        if (queued !== undefined) return { done: false, value: queued };
        if (this.#closed) return { done: true, value: undefined };
        return await new Promise((resolve) => this.#waiters.push(resolve));
      },
    };
  }

  async close(): Promise<void> {
    await this.#terminate("normal", "event session closed by caller", 1000);
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  async #establish(hello: JsonObject): Promise<void> {
    this.#state.assertOpen();
    const handshakeDeadlineMs = Date.now() + this.#handshakeTimeoutMs;
    this.#handshakeAttempt += 1;
    const attempt = this.#handshakeAttempt;
    this.#logHandshake({
      operation: "websocket.handshake",
      outcome: "started",
      attempt,
      route: "/v1/ws",
    });
    let socket: WebSocketLike | undefined;
    try {
      const encodedHello = this.#encodeOutbound(hello);
      const credentials = await this.#handshakeStep(
        this.#socketOptions(),
        handshakeDeadlineMs,
      );
      this.#validateSocketOptions(credentials);
      const socketOptions: AuthenticatedWebSocketOptions = Object.freeze({
        ...credentials,
        maxMessageBytes: this.#configuredMaxMessageBytes,
        handshakeDeadlineMs,
      });
      if (Date.now() >= handshakeDeadlineMs) {
        throw new XenoteerError(
          "websocket_timeout",
          "timed out before opening WebSocket transport",
        );
      }
      socket = this.#factory(socketOptions);
      this.#socket = socket;
      const welcomePromise = new Promise<Welcome>((resolve, reject) => {
        let opened = false;
        socket?.addEventListener("open", () => {
          opened = true;
          try {
            socket?.send(encodedHello);
          } catch (cause) {
            reject(new XenoteerError(
              "transport",
              "failed to send WebSocket hello",
              { cause },
            ));
          }
        }, { once: true });
        socket?.addEventListener("message", (event) => {
          void this.#decodeMessage(socket as WebSocketLike, event.data).then((message) => {
            if (message?.["type"] === "error") {
              reject(this.#errorFromServerMessage(message));
              return;
            }
            if (message?.["type"] !== "server.welcome") return;
            try {
              const welcome = this.#validateWelcome(message, hello);
              resolve(welcome);
            } catch (error) {
              reject(error);
            }
          }).catch((error: unknown) => {
            reject(error);
          });
        });
        socket?.addEventListener("error", () => {
          reject(new XenoteerError("transport", "WebSocket connection failed"));
        }, { once: true });
        socket?.addEventListener("close", (event) => {
          const code = event?.code;
          reject(
            code === 4401
              ? new XenoteerError(
                  "authentication",
                  "WebSocket authentication failed before welcome",
                )
              : code === 4403 || code === 1008
                ? new XenoteerError(
                    "permission",
                    "WebSocket policy rejected the connection before welcome",
                  )
                : this.#reconnectableClose(code)
                  ? new XenoteerError(
                      "transport",
                      opened
                        ? "WebSocket closed before welcome"
                        : "WebSocket transport did not open",
                    )
                  : new TerminalHandshakeCloseError(code as number, opened),
          );
        }, { once: true });
      });
      const welcome = await this.#handshakeStep(
        welcomePromise,
        handshakeDeadlineMs,
      );
      socket.addEventListener("message", (event) => {
        void this.#onMessage(socket as WebSocketLike, event.data);
      });
      socket.addEventListener("error", () => {
        if (socket === this.#socket && !this.#closed && !this.#intentionalClose) {
          void this.#handleTransportLoss("WebSocket transport error");
        }
      }, { once: true });
      socket.addEventListener("close", (event) => {
        void this.#onSocketClose(socket as WebSocketLike, event);
      }, { once: true });
      const previous = this.#welcome;
      if (
        previous !== undefined
        && (
          previous.desktopId !== welcome.desktopId
          || previous.desktopGeneration !== welcome.desktopGeneration
        )
      ) {
        this.#state.observeDesktop(
          welcome.desktopId,
          welcome.desktopGeneration,
        );
        this.#enqueueResync("generation_changed", undefined, undefined, welcome.raw);
        throw new XenoteerError(
          "outcome_unknown_after_restart",
          "desktop generation changed while reconnecting",
        );
      }
      this.#welcome = welcome;
      const generationChanged = this.#state.observeDesktop(
        welcome.desktopId,
        welcome.desktopGeneration,
      );
      if (generationChanged && previous === undefined) {
        throw new XenoteerError(
          "outcome_unknown_after_restart",
          "desktop generation changed before event session establishment",
        );
      }
      if (welcome.resumeStatus === "resync_required") {
        this.#enqueueResync("resume_rejected", undefined, undefined, welcome.raw);
        await this.#terminate(
          "resync_required",
          "server rejected the event resume cursor; refresh authoritative snapshots",
          1008,
        );
      } else {
        this.#startHeartbeat(welcome.heartbeatMs);
      }
      this.#logHandshake({
        operation: "websocket.handshake",
        outcome: "succeeded",
        attempt,
        route: "/v1/ws",
      });
    } catch (cause) {
      await this.#closeFailedCandidate(socket);
      const failure = cause instanceof XenoteerError
        ? cause
        : new XenoteerError("transport", "WebSocket session establishment failed", {
            cause,
          });
      this.#logHandshake({
        operation: "websocket.handshake",
        outcome: "failed",
        attempt,
        route: "/v1/ws",
        errorCode: failure.code,
        ...(failure.status === undefined ? {} : { status: failure.status }),
      });
      throw failure;
    }
  }

  async #handshakeStep<T>(
    operation: Promise<T>,
    deadlineMs: number,
  ): Promise<T> {
    const remainingMs = deadlineMs - Date.now();
    if (remainingMs <= 0) {
      throw new XenoteerError(
        "websocket_timeout",
        "WebSocket handshake deadline expired",
      );
    }
    return await new Promise<T>((resolve, reject) => {
      let settled = false;
      const signals = [this.#cancellation.signal, this.#state.signal];
      const listeners = new Map<AbortSignal, () => void>();
      const timer = setTimeout(() => {
        finish(() => reject(new XenoteerError(
          "websocket_timeout",
          "WebSocket handshake deadline expired",
        )));
      }, remainingMs);
      const finish = (callback: () => void): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        for (const [signal, listener] of listeners) {
          signal.removeEventListener("abort", listener);
        }
        callback();
      };
      for (const signal of signals) {
        const listener = (): void => {
          const reason = signal.reason instanceof XenoteerError
            ? signal.reason
            : new XenoteerError("session_closed", "WebSocket session cancelled");
          finish(() => reject(reason));
        };
        listeners.set(signal, listener);
        if (signal.aborted) {
          listener();
          return;
        }
        signal.addEventListener("abort", listener, { once: true });
      }
      void operation.then(
        (value) => finish(() => resolve(value)),
        (error: unknown) => finish(() => reject(error)),
      );
    });
  }

  async #decodeMessage(
    socket: WebSocketLike,
    data: unknown,
  ): Promise<Record<string, unknown> | undefined> {
    if (socket !== this.#socket || this.#closed) return undefined;
    const limit = Math.min(
      this.#configuredMaxMessageBytes,
      this.#welcome?.maxMessageBytes ?? this.#configuredMaxMessageBytes,
    );
    const text = await boundedText(data, limit);
    let decoded: unknown;
    try {
      decoded = JSON.parse(text) as unknown;
    } catch (cause) {
      throw new XenoteerError("invalid_response", "WebSocket message is not valid JSON", { cause });
    }
    if (!isObject(decoded) || typeof decoded["type"] !== "string") {
      throw new XenoteerError("invalid_response", "WebSocket message envelope is invalid");
    }
    return decoded;
  }

  async #onMessage(socket: WebSocketLike, data: unknown): Promise<void> {
    try {
      const decoded = await this.#decodeMessage(socket, data);
      if (decoded === undefined || decoded["type"] === "server.welcome") return;
      const type = decoded["type"];
      if (typeof decoded["request_id"] === "string") {
        const pending = this.#pending.get(decoded["request_id"]);
        if (pending !== undefined) {
          if (type === "error") {
            this.#settlePendingError(decoded["request_id"], decoded);
            if (this.#permanentServerError(decoded)) {
              await this.#terminate(
                "transport",
                "permanent WebSocket authorization failure",
                1008,
              );
            }
            return;
          }
          if (type === pending.expectedType) {
            if (
              pending.expectedNonce !== undefined
              && decoded["nonce"] !== pending.expectedNonce
            ) {
              throw new XenoteerError("invalid_response", "heartbeat pong nonce did not match");
            }
            clearTimeout(pending.timer);
            this.#pending.delete(decoded["request_id"]);
            pending.resolve(deepFreeze(structuredClone(decoded)));
            return;
          }
        }
      }
      if (type === "error") {
        const error = this.#errorFromServerMessage(decoded);
        if (this.#permanentServerError(decoded)) {
          await this.#terminate(
            decoded["code"] === "permission_denied" ? "transport" : "transport",
            "permanent WebSocket authorization failure",
            1008,
          );
          return;
        }
        throw error;
      }
      switch (type) {
        case "event": {
          const event = decodeEventMessage(decoded);
          const welcome = this.#welcome;
          if (welcome === undefined) {
            throw new XenoteerError("stale_reference", "event belongs to another desktop generation");
          }
          this.#validateActiveSubscription(decoded, event.topic);
          if (event.raw.event.desktop_id !== welcome.desktopId) {
            throw new XenoteerError("stale_reference", "event belongs to another desktop generation");
          }
          if (event.raw.event.desktop_generation !== welcome.desktopGeneration) {
            this.#state.invalidateDesktopGeneration(welcome.desktopId);
            throw new XenoteerError("stale_reference", "event belongs to another desktop generation");
          }
          if (this.#lastSequence !== undefined && event.sequence === this.#lastSequence) return;
          if (this.#lastSequence !== undefined && event.sequence < this.#lastSequence) {
            this.#enqueueResync(
              "sequence_regression",
              event.sequence,
              this.#lastSequence,
              decoded,
            );
            await this.#terminate(
              "resync_required",
              "event sequence regressed; refresh authoritative snapshots",
              1008,
            );
            return;
          }
          if (this.#enqueue(event)) this.#lastSequence = event.sequence;
          return;
        }
        case "events.replay_complete": {
          this.#validateContinuityScope(decoded);
          const through = decodeUInt64(decoded["through_sequence"]);
          if (this.#lastSequence !== undefined && through < this.#lastSequence) {
            this.#enqueueResync(
              "sequence_regression",
              through,
              this.#lastSequence,
              decoded,
            );
            await this.#terminate(
              "resync_required",
              "event replay sequence regressed; refresh authoritative snapshots",
              1008,
            );
            return;
          }
          if (this.#lastSequence === undefined || through > this.#lastSequence) {
            this.#lastSequence = through;
          }
          this.#enqueueReserved({
            kind: "replay_complete",
            throughSequence: through,
            raw: deepFreeze(structuredClone(decoded)),
          });
          return;
        }
        case "events.resync_required": {
          const welcome = this.#validateContinuityScope(decoded);
          if (
            typeof decoded["reason"] !== "string"
            || !EVENT_RESYNC_REASONS.has(decoded["reason"])
          ) {
            throw new XenoteerError(
              "invalid_response",
              "event resynchronization reason is invalid",
            );
          }
          const dropped = decodeUInt64(decoded["dropped_through"]);
          const latest = decodeUInt64(decoded["latest_sequence"]);
          if (decoded["reason"] === "generation_changed") {
            this.#state.invalidateDesktopGeneration(welcome.desktopId);
          }
          this.#enqueueResync(
            decoded["reason"],
            dropped,
            latest,
            decoded,
          );
          await this.#terminate(
            "resync_required",
            "server cannot preserve event continuity; refresh authoritative snapshots",
            1008,
          );
          return;
        }
        case "server.draining":
          await this.#terminate("server_draining", "server is draining", 1001);
          return;
        case "server.pong":
          return;
        default:
          this.#enqueue({
            kind: "unknown_message",
            raw: deepFreeze(structuredClone(decoded)),
          });
      }
    } catch (error) {
      const tooLarge = error instanceof XenoteerError && error.code === "response_too_large";
      await this.#terminate(
        tooLarge ? "message_too_large" : "invalid_message",
        tooLarge ? "WebSocket message exceeded the configured bound" : "WebSocket message was malformed",
        1009,
      );
    }
  }

  #validateWelcome(value: Record<string, unknown>, hello: JsonObject): Welcome {
    const protocol = value["protocol"];
    const principal = value["principal"];
    const desktop = value["desktop"];
    const limits = value["limits"];
    const resume = value["resume"];
    if (
      !isObject(protocol)
      || !Number.isInteger(protocol["major"])
      || !Number.isInteger(protocol["minor"])
      || !isObject(principal)
      || !validBoundedText(principal["id"], 256)
      || !Array.isArray(principal["capabilities"])
      || principal["capabilities"].length > 256
      || principal["capabilities"].some(
        (capability) => !validBoundedText(capability, 128),
      )
      || new Set(principal["capabilities"]).size !== principal["capabilities"].length
      || !isObject(desktop)
      || typeof desktop["state"] !== "string"
      || !DESKTOP_STATES.has(desktop["state"])
      || !isObject(limits)
      || !isObject(resume)
      || !["not_requested", "replayed", "resync_required"].includes(
        String(resume["status"]),
      )
      || !Number.isSafeInteger(limits["heartbeat_ms"])
      || (limits["heartbeat_ms"] as number) < 100
      || (limits["heartbeat_ms"] as number) > 300_000
      || !Number.isSafeInteger(limits["max_message_bytes"])
      || (limits["max_message_bytes"] as number) < 1024
      || (limits["max_message_bytes"] as number) > 16 * 1_048_576
      || !validUnsignedCapacity(limits["normal_outbound_capacity"])
      || !validUnsignedCapacity(limits["reserved_outbound_capacity"])
      || !validUnsignedCapacity(limits["max_command_watches"], true)
    ) {
      throw new XenoteerError("invalid_response", "server welcome is invalid");
    }
    validateUuid(value["connection_id"], "connection ID");
    validateUuid(desktop["id"], "welcome desktop ID");
    validateUuid(desktop["generation"], "welcome desktop generation");
    const helloProtocol = this.#baseHello["protocol"];
    if (
      !isObject(helloProtocol)
      || protocol["major"] !== helloProtocol["major"]
      || protocol["minor"] !== helloProtocol["min_minor"]
      || protocol["minor"] !== helloProtocol["max_minor"]
    ) {
      throw new XenoteerError("unsupported_protocol", "WebSocket selected an unexpected protocol");
    }
    const resumeRequested = hello["resume"] !== null && hello["resume"] !== undefined;
    if (
      (resumeRequested && resume["status"] === "not_requested")
      || (!resumeRequested && resume["status"] !== "not_requested")
    ) {
      throw new XenoteerError(
        "invalid_response",
        "server welcome resume disposition contradicts the client hello",
      );
    }
    return {
      protocol: { major: protocol["major"] as number, minor: protocol["minor"] as number },
      desktopId: desktop["id"],
      desktopGeneration: desktop["generation"],
      heartbeatMs: limits["heartbeat_ms"] as number,
      maxMessageBytes: limits["max_message_bytes"] as number,
      resumeStatus: resume["status"] as Welcome["resumeStatus"],
      raw: deepFreeze(structuredClone(value)),
    };
  }

  #validateContinuityScope(value: Record<string, unknown>): Welcome {
    const welcome = this.#welcome;
    this.#validateActiveSubscription(value);
    if (
      welcome === undefined
      || value["desktop_id"] !== welcome.desktopId
      || value["desktop_generation"] !== welcome.desktopGeneration
    ) {
      throw new XenoteerError("stale_reference", "event continuity message changed generation");
    }
    return welcome;
  }

  #validateActiveSubscription(
    value: Record<string, unknown>,
    topic?: string,
  ): void {
    const subscription = this.#subscription;
    const requestId = this.#subscriptionRequestId;
    if (
      subscription === undefined
      || requestId === undefined
      || value["request_id"] !== requestId
      || (
        topic !== undefined
        && subscription.topics.length > 0
        && !subscription.topics.includes(topic)
      )
    ) {
      throw new XenoteerError(
        "invalid_response",
        "event message does not match the active subscription",
      );
    }
  }

  #expect(requestId: string, expectedType: string): Promise<Readonly<Record<string, unknown>>> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(requestId);
        reject(new XenoteerError("websocket_timeout", `timed out waiting for ${expectedType}`));
      }, this.#ackTimeoutMs);
      this.#pending.set(requestId, { expectedType, resolve, reject, timer });
    });
  }

  #settlePendingError(requestId: string, message: Record<string, unknown>): void {
    const pending = this.#pending.get(requestId);
    if (pending === undefined) return;
    clearTimeout(pending.timer);
    this.#pending.delete(requestId);
    pending.reject(this.#errorFromServerMessage(message, requestId));
  }

  #send(message: JsonObject): void {
    this.#ensureOpen();
    this.#socket?.send(this.#encodeOutbound(message));
  }

  #encodeOutbound(message: JsonObject): string {
    const encoded = JSON.stringify(message);
    if (utf8Length(encoded, this.#configuredMaxMessageBytes) > this.#configuredMaxMessageBytes) {
      throw new XenoteerError("request_too_large", "WebSocket request exceeds configured bound");
    }
    return encoded;
  }

  #startHeartbeat(intervalMs: number): void {
    if (this.#heartbeatTimer !== undefined) clearInterval(this.#heartbeatTimer);
    this.#heartbeatTimer = setInterval(() => {
      if (this.#closed || this.#reconnecting) return;
      const requestId = globalThis.crypto.randomUUID();
      const nonce = `heartbeat-${requestId}`;
      const watchdog = setTimeout(() => {
        if (this.#pending.has(requestId)) {
          this.#pending.delete(requestId);
          void this.#handleTransportLoss("heartbeat timeout", "heartbeat_timeout");
        }
      }, intervalMs + this.#heartbeatGraceMs);
      this.#pending.set(requestId, {
        expectedType: "server.pong",
        expectedNonce: nonce,
        resolve: () => clearTimeout(watchdog),
        reject: () => clearTimeout(watchdog),
        timer: watchdog,
      });
      try {
        this.#send({ type: "client.ping", request_id: requestId, nonce });
      } catch {
        clearTimeout(watchdog);
        this.#pending.delete(requestId);
        void this.#handleTransportLoss("heartbeat send failed");
      }
    }, intervalMs);
  }

  async #onSocketClose(
    socket: WebSocketLike,
    event?: { readonly code?: number; readonly reason?: string },
  ): Promise<void> {
    if (
      socket !== this.#socket
      || this.#closed
      || this.#intentionalClose
      || this.#failedCandidatesClosed.has(socket)
    ) {
      return;
    }
    if (!this.#reconnectableClose(event?.code)) {
      await this.#terminate(
        "transport",
        event?.code === undefined
          ? "WebSocket closed without a reconnectable close disposition"
          : `WebSocket closed with terminal code ${event.code}`,
        1008,
      );
      return;
    }
    await this.#handleTransportLoss(
      event?.code === undefined ? "WebSocket transport closed" : `WebSocket closed (${event.code})`,
    );
  }

  async #handleTransportLoss(
    detail: string,
    terminalCode: EventSessionTerminalReason["code"] = "transport",
  ): Promise<void> {
    if (this.#closed || this.#reconnecting) return;
    this.#reconnecting = true;
    if (this.#heartbeatTimer !== undefined) clearInterval(this.#heartbeatTimer);
    this.#rejectPending(new XenoteerError("transport", "WebSocket transport was interrupted"));
    const failedSocket = this.#socket;
    if (failedSocket !== undefined) {
      this.#socket = undefined;
      await this.#closeFailedCandidate(failedSocket, 1011, "transport_lost");
    }
    for (let attempt = 0; attempt < this.#maxReconnectAttempts; attempt += 1) {
      if (this.#closed || this.#state.closed) return;
      const baseDelay = Math.min(
        this.#maxReconnectDelayMs,
        this.#initialReconnectDelayMs * (2 ** attempt),
      );
      const jitterBound = Math.max(1, Math.floor(baseDelay / 5));
      const delay = Math.min(
        this.#maxReconnectDelayMs,
        baseDelay + Math.floor(Math.random() * jitterBound),
      );
      if (!await this.#waitForReconnectDelay(delay)) return;
      const welcome = this.#welcome;
      const hello = structuredClone(this.#baseHello) as Record<string, JsonValue>;
      if (welcome !== undefined && this.#lastSequence !== undefined) {
        hello["resume"] = {
          desktop_id: welcome.desktopId,
          desktop_generation: welcome.desktopGeneration,
          event_sequence: this.#lastSequence.toString(10),
        };
      }
      try {
        await this.#establish(hello);
        if (this.#closed) return;
        const subscription = this.#subscription;
        if (subscription !== undefined) {
          await this.subscribe(
            subscription.topics,
            this.#lastSequence === undefined
              ? subscription.sinceSequence
              : this.#lastSequence.toString(10) as CanonicalUInt64,
          );
        }
        await this.#state.notifyReconnect();
        this.#reconnecting = false;
        return;
      } catch (error) {
        if (this.#closed) return;
        if (this.#permanentEstablishError(error)) {
          this.#reconnecting = false;
          await this.#terminate(
            error instanceof XenoteerError
              && (
                error.code === "outcome_unknown_after_restart"
                || error.code === "generation_changed"
                || error.code === "stale_reference"
              )
              ? "generation_changed"
              : "transport",
            "WebSocket reconnect encountered a permanent handshake failure",
            1008,
          );
          return;
        }
        await this.#closeFailedCandidate(this.#socket);
        // Continue bounded transient reconnect attempts; mutations are never replayed.
      }
    }
    this.#reconnecting = false;
    await this.#terminate(terminalCode, detail, 1011);
  }

  async #waitForReconnectDelay(delayMs: number): Promise<boolean> {
    if (this.#closed || this.#state.closed) return false;
    return await new Promise<boolean>((resolve) => {
      let settled = false;
      const signals = [this.#cancellation.signal, this.#state.signal];
      const listeners = new Map<AbortSignal, () => void>();
      const finish = (value: boolean): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        for (const [signal, listener] of listeners) {
          signal.removeEventListener("abort", listener);
        }
        resolve(value);
      };
      const timer = setTimeout(() => finish(true), delayMs);
      for (const signal of signals) {
        const listener = (): void => finish(false);
        listeners.set(signal, listener);
        if (signal.aborted) {
          listener();
          return;
        }
        signal.addEventListener("abort", listener, { once: true });
      }
    });
  }

  #enqueue(event: XenoteerEvent): boolean {
    const waiter = this.#waiters.shift();
    if (waiter !== undefined) {
      waiter({ done: false, value: event });
      return true;
    }
    if (this.#ordinaryQueued >= this.#capacity) {
      void this.#terminate(
        "backpressure",
        "bounded event queue overflowed; authoritative refresh is required",
        1008,
      );
      return false;
    }
    this.#queue.push(event);
    this.#ordinaryQueued += 1;
    return true;
  }

  #enqueueReserved(event: ReplayCompleteEvent | ResyncRequiredEvent): boolean {
    const waiter = this.#waiters.shift();
    if (waiter !== undefined) {
      waiter({ done: false, value: event });
      return true;
    }
    if (this.#reservedQueued >= RESERVED_EVENT_CAPACITY) {
      if (event.kind === "resync_required") {
        const replayIndex = this.#queue.findIndex(
          (queued) => queued.kind === "replay_complete",
        );
        if (replayIndex >= 0) {
          this.#queue.splice(replayIndex, 1);
          this.#reservedQueued -= 1;
        }
      }
      if (this.#reservedQueued >= RESERVED_EVENT_CAPACITY) {
        void this.#terminate(
          "backpressure",
          "reserved event queue overflowed; authoritative refresh is required",
          1008,
        );
        return false;
      }
    }
    this.#queue.push(event);
    this.#reservedQueued += 1;
    return true;
  }

  #dequeue(): XenoteerEvent | undefined {
    const event = this.#queue.shift();
    if (event === undefined) return undefined;
    if (event.kind === "replay_complete" || event.kind === "resync_required") {
      this.#reservedQueued -= 1;
    } else {
      this.#ordinaryQueued -= 1;
    }
    return event;
  }

  #enqueueResync(
    reason: string,
    droppedThrough: bigint | undefined,
    latestSequence: bigint | undefined,
    raw: Record<string, unknown> | Readonly<Record<string, unknown>>,
  ): void {
    this.#enqueueReserved({
      kind: "resync_required",
      reason,
      ...(droppedThrough === undefined ? {} : { droppedThrough }),
      ...(latestSequence === undefined ? {} : { latestSequence }),
      raw: deepFreeze(structuredClone(raw)),
    });
  }

  #logHandshake(event: SafeLogEvent): void {
    if (this.#safeLog === undefined) return;
    try {
      this.#safeLog(Object.freeze({
        operation: event.operation,
        outcome: event.outcome,
        ...(event.attempt === undefined ? {} : { attempt: event.attempt }),
        route: "/v1/ws",
        ...(event.status === undefined ? {} : { status: event.status }),
        ...(event.errorCode === undefined ? {} : { errorCode: event.errorCode }),
      }));
    } catch {
      // A diagnostic hook cannot alter connection or retry behavior.
    }
  }

  async #closeFailedCandidate(
    socket: WebSocketLike | undefined,
    code = 1002,
    reason = "handshake_failed",
  ): Promise<void> {
    if (
      socket === undefined
      || socket === null
      || typeof socket !== "object"
      || this.#failedCandidatesClosed.has(socket)
    ) {
      return;
    }
    this.#failedCandidatesClosed.add(socket);
    try {
      socket.close(code, reason);
    } catch {
      // Candidate ownership still ends even if the adapter's close throws.
    }
    await Promise.resolve();
  }

  async #terminate(
    code: EventSessionTerminalReason["code"],
    detail: string,
    closeCode: number,
  ): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#cancellation.abort(
      new XenoteerError("session_closed", detail),
    );
    this.#intentionalClose = true;
    this.#terminal = Object.freeze({ code, detail });
    if (this.#heartbeatTimer !== undefined) clearInterval(this.#heartbeatTimer);
    this.#rejectPending(new XenoteerError("session_closed", detail));
    this.#unregisterStateClose();
    const socket = this.#socket;
    try {
      if (
        socket !== undefined
        && socket.readyState !== 3
        && !this.#failedCandidatesClosed.has(socket)
      ) {
        const closed = new Promise<void>((resolve) => {
          const timer = setTimeout(resolve, this.#closeTimeoutMs);
          socket.addEventListener("close", () => {
            clearTimeout(timer);
            resolve();
          }, { once: true });
        });
        socket.close(closeCode, code);
        await closed;
      }
    } catch {
      // The local terminal state is authoritative even if socket.close throws.
    }
    for (const waiter of this.#waiters.splice(0)) {
      waiter({ done: true, value: undefined });
    }
  }

  #rejectPending(error: XenoteerError): void {
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
  }

  #ensureOpen(): void {
    this.#state.assertOpen();
    if (this.#closed) {
      throw new XenoteerError("session_closed", "event session is closed");
    }
  }

  #validateSocketOptions(options: WebSocketCredentials): void {
    if (!options.url.startsWith("ws://") && !options.url.startsWith("wss://")) {
      throw new XenoteerError("invalid_request", "WebSocket URL must use ws or wss");
    }
    const parsed = new URL(options.url);
    if (
      parsed.username !== ""
      || parsed.password !== ""
      || parsed.search !== ""
      || parsed.hash !== ""
    ) {
      throw new XenoteerError(
        "invalid_request",
        "WebSocket URL must not contain credentials, query, or fragment",
      );
    }
    if (!/^Bearer [A-Za-z0-9._~+/-]+={0,}$/u.test(options.authorization)) {
      throw new XenoteerError("invalid_token", "WebSocket bearer credential is invalid");
    }
  }

  #reconnectableClose(code: number | undefined): boolean {
    return code === undefined || code === 1001 || code === 1012 || code === 1013;
  }

  #permanentServerError(message: Record<string, unknown>): boolean {
    return message["status"] === 401
      || message["status"] === 403
      || message["code"] === "authentication_required"
      || message["code"] === "permission_denied";
  }

  #permanentEstablishError(error: unknown): boolean {
    return error instanceof TerminalHandshakeCloseError
      || (
        error instanceof XenoteerError
        && [
          "authentication",
          "permission",
          "unsupported_protocol",
          "unsupported_major",
          "no_shared_minor",
          "unsupported_version",
          "generation_changed",
          "stale_reference",
          "outcome_unknown_after_restart",
        ].includes(error.code)
      );
  }

  #errorFromServerMessage(
    message: Record<string, unknown>,
    requestId?: string,
  ): XenoteerError {
    const code = typeof message["code"] === "string"
      ? message["code"]
      : undefined;
    const status = Number.isInteger(message["status"])
      ? message["status"] as number
      : undefined;
    const sdkCode = status === 401 || code === "authentication_required"
      ? "authentication"
      : status === 403 || code === "permission_denied"
        ? "permission"
        : [
            "unsupported_protocol",
            "unsupported_version",
            "unsupported_major",
            "no_shared_minor",
          ].includes(code ?? "")
          ? "unsupported_protocol"
          : [
              "generation_changed",
              "generation_mismatch",
              "stale_reference",
            ].includes(code ?? "")
            ? "generation_changed"
        : "unexpected_http_status";
    return new XenoteerError(
      sdkCode,
      code === undefined
        ? "WebSocket request failed"
        : `WebSocket request failed (${code})`,
      {
        ...(requestId === undefined ? {} : { requestId }),
        ...(status === undefined ? {} : { status }),
        ...(code === undefined ? {} : { problemCode: code }),
      },
    );
  }
}
