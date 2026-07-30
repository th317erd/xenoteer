// SPDX-License-Identifier: Apache-2.0

import { errorFromProblem, XenoteerError } from "./errors.js";
import {
  BearerToken,
  type SafeLogEvent,
  type SafeLogHook,
  type SafeLogOperation,
  type SafeLogRoute,
  type TokenSource,
} from "./options.js";
import type { ApiProblem, JsonValue } from "./protocol.generated.js";

const DEFAULT_TIMEOUT_MS = 35_000;
const DEFAULT_JSON_BYTES = 1_048_576;
const DEFAULT_ARTIFACT_BYTES = 33_554_432;
const MAX_JSON_BYTES = 32 * 1_048_576;
const MAX_ARTIFACT_BYTES = 256 * 1_048_576;

export interface TransportOptions {
  readonly baseUrl: string;
  readonly token: TokenSource;
  readonly requestTimeoutMs?: number;
  readonly maxResponseBytes?: number;
  readonly maxArtifactBytes?: number;
  readonly fetch?: typeof fetch;
  readonly log?: SafeLogHook;
}

export interface RequestOptions {
  readonly signal?: AbortSignal;
  readonly headers?: Readonly<Record<string, string>>;
  readonly timeoutMs?: number;
  readonly maxRequestBytes?: number;
  readonly maxResponseBytes?: number;
}

export interface ByteResponse {
  readonly bytes: Uint8Array;
  readonly contentType: string;
  readonly status: number;
  readonly headers: Headers;
}

export interface ByteStreamResponse {
  readonly bytes: AsyncIterable<Uint8Array>;
  readonly contentType: string;
  readonly status: number;
  readonly headers: Headers;
  /** @internal Artifact validation must confirm length and digest after EOF. */
  readonly markVerifiedEof: () => void;
  /** @internal Artifact validation reports failures without exposing details. */
  readonly markFailed: (errorCode: XenoteerError["code"]) => void;
  /** @internal Rejects metadata before iteration and cancels the response body. */
  readonly abortBeforeRead: (errorCode: XenoteerError["code"]) => Promise<void>;
}

export interface ArtifactResponseExpectation {
  readonly contentLength: number;
  readonly sha256: string;
}

export type CloseHandler = () => void | Promise<void>;
export type ReconnectHandler = () => void | Promise<void>;

/** One lifecycle fence shared by the root, desktops, handles, leases, and sessions. */
export class ClientConnectionState {
  #closed = false;
  readonly #cancellation = new AbortController();
  readonly #handlers = new Set<CloseHandler>();
  readonly #reconnectHandlers = new Set<ReconnectHandler>();
  #desktopId?: string;
  #desktopGeneration: string | undefined;

  get closed(): boolean {
    return this.#closed;
  }

  get signal(): AbortSignal {
    return this.#cancellation.signal;
  }

  assertOpen(): void {
    if (this.#closed) {
      throw new XenoteerError("client_closed", "Xenoteer client is closed");
    }
  }

  register(handler: CloseHandler): () => void {
    this.assertOpen();
    this.#handlers.add(handler);
    return () => this.#handlers.delete(handler);
  }

  registerReconnect(handler: ReconnectHandler): () => void {
    this.assertOpen();
    this.#reconnectHandlers.add(handler);
    return () => this.#reconnectHandlers.delete(handler);
  }

  bindDesktop(desktopId: string, desktopGeneration: string): void {
    this.assertOpen();
    this.#desktopId = desktopId;
    this.#desktopGeneration = desktopGeneration;
  }

  observeDesktop(desktopId: string, desktopGeneration: string): boolean {
    this.assertOpen();
    const changed = this.#desktopId !== undefined
      && (
        this.#desktopId !== desktopId
        || this.#desktopGeneration !== desktopGeneration
      );
    this.#desktopId = desktopId;
    this.#desktopGeneration = desktopGeneration;
    return changed;
  }

  invalidateDesktopGeneration(desktopId: string): boolean {
    this.assertOpen();
    if (this.#desktopId !== desktopId) return false;
    this.#desktopGeneration = undefined;
    return true;
  }

  assertGeneration(
    desktopId: string,
    desktopGeneration: string,
    kind: "command" | "lease" | "reference" = "reference",
  ): void {
    this.assertOpen();
    if (
      this.#desktopId === desktopId
      && this.#desktopGeneration === desktopGeneration
    ) {
      return;
    }
    if (kind === "command") {
      throw new XenoteerError(
        "generation_changed",
        "desktop generation changed; command was not submitted",
      );
    }
    if (kind === "lease") {
      throw new XenoteerError(
        "lease_released",
        "desktop restarted; the controller lease is no longer valid",
      );
    }
    throw new XenoteerError(
      "stale_reference",
      "desktop restarted; generation-bound reference is stale",
    );
  }

  async notifyReconnect(): Promise<void> {
    this.assertOpen();
    await Promise.allSettled(
      [...this.#reconnectHandlers].map(async (handler) => await handler()),
    );
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#cancellation.abort(
      new XenoteerError("client_closed", "Xenoteer client is closed"),
    );
    const handlers = [...this.#handlers];
    this.#handlers.clear();
    this.#reconnectHandlers.clear();
    await Promise.allSettled(handlers.map(async (handler) => await handler()));
  }
}

const RESERVED_CALLER_HEADERS = new Set([
  "accept",
  "authorization",
  "content-length",
  "content-type",
  "x-content-sha256",
]);

function mergeRequestHeaders(
  sdkHeaders: Readonly<Record<string, string>>,
  callerHeaders: Readonly<Record<string, string>> | undefined,
): Readonly<Record<string, string>> {
  if (callerHeaders === undefined) return sdkHeaders;
  for (const name of Object.keys(callerHeaders)) {
    if (RESERVED_CALLER_HEADERS.has(name.toLowerCase())) {
      throw new XenoteerError(
        "invalid_request",
        "caller headers cannot override SDK authority or framing",
      );
    }
  }
  return { ...sdkHeaders, ...callerHeaders };
}

async function withCancellation<T>(
  operation: Promise<T>,
  signals: readonly AbortSignal[],
): Promise<T> {
  return await new Promise<T>((resolve, reject) => {
    let settled = false;
    const listeners = new Map<AbortSignal, () => void>();
    const finish = (callback: () => void): void => {
      if (settled) return;
      settled = true;
      for (const [signal, listener] of listeners) {
        signal.removeEventListener("abort", listener);
      }
      callback();
    };
    for (const signal of signals) {
      const listener = (): void => {
        const reason = signal.reason instanceof XenoteerError
          ? signal.reason
          : new XenoteerError("request_cancelled", "local SDK operation cancelled");
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

function validatePositiveInteger(value: number, maximum: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > maximum) {
    throw new XenoteerError("invalid_request", `${label} is outside its supported range`);
  }
  return value;
}

function isNumericLoopback(hostname: string): boolean {
  if (hostname === "[::1]" || hostname === "::1") return true;
  const parts = hostname.split(".");
  if (parts.length !== 4) return false;
  const octets = parts.map((part) => {
    if (!/^(?:0|[1-9][0-9]{0,2})$/u.test(part)) return null;
    const value = Number(part);
    return value <= 255 ? value : null;
  });
  return octets.every((part) => part !== null) && octets[0] === 127;
}

export function normalizeBaseUrl(value: string): string {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new XenoteerError("invalid_base_url", "baseUrl must be an absolute HTTP(S) origin");
  }
  if (
    !["http:", "https:"].includes(parsed.protocol)
    || parsed.username !== ""
    || parsed.password !== ""
    || parsed.pathname !== "/"
    || parsed.search !== ""
    || parsed.hash !== ""
  ) {
    throw new XenoteerError(
      "invalid_base_url",
      "baseUrl must be an HTTP(S) origin without credentials, path, query, or fragment",
    );
  }
  if (parsed.protocol === "http:" && !isNumericLoopback(parsed.hostname)) {
    throw new XenoteerError(
      "invalid_base_url",
      "plaintext HTTP is allowed only for a numeric loopback address",
    );
  }
  return parsed.origin;
}

function contentType(response: Response): string {
  return (response.headers.get("content-type") ?? "")
    .split(";", 1)[0]?.trim().toLowerCase() ?? "";
}

async function collectBounded(response: Response, limit: number): Promise<Uint8Array> {
  const declared = response.headers.get("content-length");
  if (declared !== null) {
    const length = Number(declared);
    if (!Number.isSafeInteger(length) || length < 0 || length > limit) {
      await response.body?.cancel();
      throw new XenoteerError("response_too_large", `response exceeds ${limit} bytes`);
    }
  }
  if (response.body === null) return new Uint8Array();
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const next = await reader.read();
    if (next.done) break;
    total += next.value.byteLength;
    if (total > limit) {
      await reader.cancel();
      throw new XenoteerError("response_too_large", `response exceeds ${limit} bytes`);
    }
    chunks.push(next.value);
  }
  const result = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

function decodeJson(bytes: Uint8Array): unknown {
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (cause) {
    throw new XenoteerError("invalid_response", "server returned invalid UTF-8 JSON", { cause });
  }
}

function validatePath(path: string): void {
  if (!path.startsWith("/") || path.startsWith("//")) {
    throw new XenoteerError("invalid_request", "request path must be origin-relative");
  }
}

/** Maps dynamic request targets to a finite metadata-only diagnostic vocabulary. */
function classifyRoute(path: string): SafeLogRoute {
  let pathname: string;
  try {
    pathname = new URL(path, "https://xenoteer.invalid").pathname;
  } catch {
    return "unknown";
  }
  if (pathname === "/v1/status") return "/v1/status";
  if (pathname === "/v1/ws") return "/v1/ws";
  if (pathname === "/v1/artifacts") return "/v1/artifacts";
  if (/^\/v1\/artifacts\/[^/]+$/u.test(pathname)) {
    return "/v1/artifacts/:artifact_id";
  }
  const desktop = "^/v1/desktops/[^/]+";
  const routes: ReadonlyArray<readonly [RegExp, SafeLogRoute]> = [
    [new RegExp(`${desktop}/commands$`, "u"), "/v1/desktops/:desktop_id/commands"],
    [
      new RegExp(`${desktop}/commands/[^/]+$`, "u"),
      "/v1/desktops/:desktop_id/commands/:command_id",
    ],
    [new RegExp(`${desktop}/lease$`, "u"), "/v1/desktops/:desktop_id/lease"],
    [
      new RegExp(`${desktop}/lease/[^/]+/renew$`, "u"),
      "/v1/desktops/:desktop_id/lease/:lease_id/renew",
    ],
    [new RegExp(`${desktop}/windows$`, "u"), "/v1/desktops/:desktop_id/windows"],
    [
      new RegExp(`${desktop}/windows/query$`, "u"),
      "/v1/desktops/:desktop_id/windows/query",
    ],
    [
      new RegExp(`${desktop}/windows/resolve$`, "u"),
      "/v1/desktops/:desktop_id/windows/resolve",
    ],
    [
      new RegExp(`${desktop}/windows/wait$`, "u"),
      "/v1/desktops/:desktop_id/windows/wait",
    ],
    [
      new RegExp(`${desktop}/windows/[^/]+$`, "u"),
      "/v1/desktops/:desktop_id/windows/:window_reference",
    ],
    [
      new RegExp(`${desktop}/accessibility/elements/list$`, "u"),
      "/v1/desktops/:desktop_id/accessibility/elements/list",
    ],
    [
      new RegExp(`${desktop}/accessibility/elements/query$`, "u"),
      "/v1/desktops/:desktop_id/accessibility/elements/query",
    ],
    [
      new RegExp(`${desktop}/accessibility/elements/resolve$`, "u"),
      "/v1/desktops/:desktop_id/accessibility/elements/resolve",
    ],
    [
      new RegExp(`${desktop}/accessibility/elements/snapshot$`, "u"),
      "/v1/desktops/:desktop_id/accessibility/elements/snapshot",
    ],
    [
      new RegExp(`${desktop}/accessibility/elements/wait$`, "u"),
      "/v1/desktops/:desktop_id/accessibility/elements/wait",
    ],
    [
      new RegExp(`${desktop}/clipboard/read$`, "u"),
      "/v1/desktops/:desktop_id/clipboard/read",
    ],
    [
      new RegExp(`${desktop}/screenshots$`, "u"),
      "/v1/desktops/:desktop_id/screenshots",
    ],
    [
      new RegExp(`${desktop}/viewer-tickets$`, "u"),
      "/v1/desktops/:desktop_id/viewer-tickets",
    ],
  ];
  return routes.find(([pattern]) => pattern.test(pathname))?.[1] ?? "unknown";
}

type SafeLogInput = {
  readonly operation: SafeLogOperation;
  readonly outcome: SafeLogEvent["outcome"];
  readonly attempt?: number;
  readonly method?: "GET" | "POST" | "DELETE";
  readonly route?: SafeLogRoute;
  readonly status?: number;
  readonly requestBytes?: number;
  readonly responseBytes?: number;
  readonly errorCode?: XenoteerError["code"];
};

/** Bounded, retry-neutral transport. Every call performs exactly one fetch. */
export class HttpTransport {
  readonly #baseUrl: string;
  readonly #token: TokenSource;
  readonly #timeoutMs: number;
  readonly #maxResponseBytes: number;
  readonly #maxArtifactBytes: number;
  readonly #fetch: typeof fetch;
  readonly #log: SafeLogHook | undefined;
  readonly state = new ClientConnectionState();

  constructor(options: TransportOptions) {
    this.#baseUrl = normalizeBaseUrl(options.baseUrl);
    this.#token = options.token;
    this.#timeoutMs = validatePositiveInteger(
      options.requestTimeoutMs ?? DEFAULT_TIMEOUT_MS,
      300_000,
      "requestTimeoutMs",
    );
    this.#maxResponseBytes = validatePositiveInteger(
      options.maxResponseBytes ?? DEFAULT_JSON_BYTES,
      MAX_JSON_BYTES,
      "maxResponseBytes",
    );
    this.#maxArtifactBytes = validatePositiveInteger(
      options.maxArtifactBytes ?? DEFAULT_ARTIFACT_BYTES,
      MAX_ARTIFACT_BYTES,
      "maxArtifactBytes",
    );
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#log = options.log;
    if (typeof this.#fetch !== "function") {
      throw new XenoteerError("invalid_request", "a Fetch-compatible implementation is required");
    }
  }

  get baseUrl(): string {
    return this.#baseUrl;
  }

  get maxArtifactBytes(): number {
    return this.#maxArtifactBytes;
  }

  async close(): Promise<void> {
    await this.state.close();
  }

  async authorizationHeader(signal?: AbortSignal): Promise<string> {
    this.state.assertOpen();
    let source: string;
    try {
      const resolution = Promise.resolve(
        typeof this.#token === "function" ? this.#token() : this.#token,
      );
      source = await withCancellation(
        resolution,
        signal === undefined
          ? [this.state.signal]
          : [this.state.signal, signal],
      );
    } catch (cause) {
      if (cause instanceof XenoteerError) throw cause;
      throw new XenoteerError("invalid_token", "Xenoteer token provider failed");
    }
    return new BearerToken(source).authorizationHeader();
  }

  safeLog(event: SafeLogInput): void {
    if (this.#log === undefined) return;
    const closed: SafeLogEvent = Object.freeze({
      operation: event.operation,
      outcome: event.outcome,
      ...(event.attempt === undefined ? {} : { attempt: event.attempt }),
      ...(event.method === undefined ? {} : { method: event.method }),
      ...(event.route === undefined ? {} : { route: event.route }),
      ...(event.status === undefined ? {} : { status: event.status }),
      ...(event.requestBytes === undefined ? {} : { requestBytes: event.requestBytes }),
      ...(event.responseBytes === undefined ? {} : { responseBytes: event.responseBytes }),
      ...(event.errorCode === undefined ? {} : { errorCode: event.errorCode }),
    });
    try {
      this.#log(closed);
    } catch {
      // Diagnostics are observational and can never break SDK behavior.
    }
  }

  async request<T extends JsonValue | object>(
    method: "GET" | "POST" | "DELETE",
    path: string,
    body?: JsonValue | object,
    options: RequestOptions = {},
  ): Promise<T> {
    const encoded = body === undefined ? undefined : JSON.stringify(body);
    return await this.requestSerialized<T>(method, path, encoded, options);
  }

  /** Sends caller-retained exact JSON bytes, enabling explicit same-ID recovery. */
  async requestSerialized<T extends JsonValue | object>(
    method: "GET" | "POST" | "DELETE",
    path: string,
    encoded?: string,
    options: RequestOptions = {},
  ): Promise<T> {
    const maxRequest = options.maxRequestBytes ?? this.#maxResponseBytes;
    if (encoded !== undefined && new TextEncoder().encode(encoded).byteLength > maxRequest) {
      throw new XenoteerError("request_too_large", `request exceeds ${maxRequest} bytes`);
    }
    const headers = mergeRequestHeaders({
      accept: "application/json, application/problem+json",
      ...(encoded === undefined ? {} : { "content-type": "application/json" }),
    }, options.headers);
    return await this.#perform<T>(
      "http.request",
      method,
      path,
      encoded,
      headers,
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
      (response, bytes) => {
        if (contentType(response) !== "application/json") {
          throw new XenoteerError(
            "invalid_response",
            "successful response was not application/json",
          );
        }
        return decodeJson(bytes) as T;
      },
    );
  }

  async upload<T = unknown>(
    path: string,
    bytes: Uint8Array,
    contentTypeValue: string,
    options: RequestOptions = {},
    validate?: (value: unknown) => T,
    integritySha256?: string,
  ): Promise<T> {
    const max = options.maxRequestBytes ?? this.#maxArtifactBytes;
    if (bytes.byteLength < 1 || bytes.byteLength > max) {
      throw new XenoteerError("request_too_large", `artifact upload must be 1..${max} bytes`);
    }
    return await this.#perform<T>(
      "artifact.upload",
      "POST",
      path,
      new Uint8Array(bytes).buffer,
      mergeRequestHeaders({
        accept: "application/json, application/problem+json",
        "content-type": contentTypeValue,
        "content-length": String(bytes.byteLength),
        ...(integritySha256 === undefined
          ? {}
          : { "x-content-sha256": integritySha256 }),
      }, options.headers),
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
      (response, responseBytes) => {
        if (contentType(response) !== "application/json") {
          throw new XenoteerError(
            "invalid_response",
            "artifact upload response was not JSON",
          );
        }
        const decoded = decodeJson(responseBytes);
        return validate === undefined ? decoded as T : validate(decoded);
      },
    );
  }

  async uploadStream<T = unknown>(
    path: string,
    stream: ReadableStream<Uint8Array>,
    contentLength: number,
    contentTypeValue: string,
    options: RequestOptions = {},
    validate?: (value: unknown) => T,
    integritySha256?: string,
  ): Promise<T> {
    const max = options.maxRequestBytes ?? this.#maxArtifactBytes;
    if (
      !Number.isSafeInteger(contentLength)
      || contentLength < 1
      || contentLength > max
    ) {
      throw new XenoteerError(
        "request_too_large",
        `artifact upload must be 1..${max} bytes`,
      );
    }
    return await this.#perform<T>(
      "artifact.upload",
      "POST",
      path,
      stream,
      mergeRequestHeaders({
        accept: "application/json, application/problem+json",
        "content-type": contentTypeValue,
        "content-length": String(contentLength),
        ...(integritySha256 === undefined
          ? {}
          : { "x-content-sha256": integritySha256 }),
      }, options.headers),
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
      (response, responseBytes) => {
        if (contentType(response) !== "application/json") {
          throw new XenoteerError(
            "invalid_response",
            "artifact upload response was not JSON",
          );
        }
        const decoded = decodeJson(responseBytes);
        return validate === undefined ? decoded as T : validate(decoded);
      },
    );
  }

  async download(path: string, options: RequestOptions = {}): Promise<ByteResponse> {
    return await this.#perform<ByteResponse>(
      "artifact.download",
      "GET",
      path,
      undefined,
      mergeRequestHeaders(
        { accept: "application/octet-stream, image/png, image/webp, image/bmp" },
        options.headers,
      ),
      options,
      options.maxResponseBytes ?? this.#maxArtifactBytes,
      (response, bytes) => ({
        bytes,
        contentType: contentType(response),
        status: response.status,
        headers: new Headers(response.headers),
      }),
    );
  }

  async downloadStream(
    path: string,
    options: RequestOptions = {},
    expected?: ArtifactResponseExpectation,
  ): Promise<ByteStreamResponse> {
    this.state.assertOpen();
    validatePath(path);
    const limit = options.maxResponseBytes ?? this.#maxArtifactBytes;
    validatePositiveInteger(limit, MAX_ARTIFACT_BYTES, "response byte limit");
    const timeoutMs = validatePositiveInteger(
      options.timeoutMs ?? this.#timeoutMs,
      3_600_000,
      "request timeout",
    );
    const requestHeaders = mergeRequestHeaders(
      { accept: "application/octet-stream, image/png, image/webp, image/bmp" },
      options.headers,
    );
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(
      new XenoteerError("request_timeout", "artifact download timed out"),
    ), timeoutMs);
    const onAbort = (): void => controller.abort(
      new XenoteerError(
        "request_cancelled",
        "local artifact download cancelled",
        { cause: options.signal?.reason },
      ),
    );
    const onStateAbort = (): void => controller.abort(this.state.signal.reason);
    if (options.signal?.aborted === true) onAbort();
    else options.signal?.addEventListener("abort", onAbort, { once: true });
    if (this.state.signal.aborted) onStateAbort();
    else this.state.signal.addEventListener("abort", onStateAbort, { once: true });
    const route = classifyRoute(path);
    this.safeLog({
      operation: "artifact.download",
      outcome: "started",
      attempt: 1,
      method: "GET",
      route,
    });
    let terminalLogged = false;
    const logTerminal = (
      outcome: "succeeded" | "failed",
      metadata: {
        readonly status?: number;
        readonly responseBytes?: number;
        readonly errorCode?: XenoteerError["code"];
      } = {},
    ): void => {
      if (terminalLogged) return;
      terminalLogged = true;
      this.safeLog({
        operation: "artifact.download",
        outcome,
        attempt: 1,
        method: "GET",
        route,
        ...metadata,
      });
    };
    const typedFailure = (cause: unknown): XenoteerError => {
      if (cause instanceof XenoteerError) return cause;
      if (controller.signal.reason instanceof XenoteerError) {
        return controller.signal.reason;
      }
      if (controller.signal.aborted) {
        return new XenoteerError(
          "request_timeout",
          "artifact download timed out",
          { cause },
        );
      }
      return new XenoteerError(
        "transport",
        "artifact download transport failed",
        { cause },
      );
    };
    let response: Response;
    try {
      response = await this.#fetch(`${this.#baseUrl}${path}`, {
        method: "GET",
        headers: new Headers({
          authorization: await this.authorizationHeader(controller.signal),
          ...requestHeaders,
        }),
        signal: controller.signal,
      });
    } catch (cause) {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      this.state.signal.removeEventListener("abort", onStateAbort);
      const failure = typedFailure(cause);
      logTerminal("failed", { errorCode: failure.code });
      throw failure;
    }
    try {
      if (!response.ok) {
        const bytes = await collectBounded(
          response,
          Math.min(this.#maxResponseBytes, 64 * 1024),
        );
        if (contentType(response) === "application/problem+json") {
          const problem = decodeJson(bytes);
          if (typeof problem === "object" && problem !== null && !Array.isArray(problem)) {
            throw errorFromProblem(response.status, problem as ApiProblem);
          }
        }
        throw new XenoteerError(
          "unexpected_http_status",
          `Xenoteer request failed with HTTP ${response.status}`,
          { status: response.status },
        );
      }
      if (expected !== undefined) {
        const declaredLength = response.headers.get("content-length");
        const declaredDigest = response.headers.get("x-content-sha256");
        if (
          declaredLength === null
          || !/^(?:0|[1-9][0-9]*)$/u.test(declaredLength)
          || Number(declaredLength) !== expected.contentLength
          || declaredDigest === null
          || !/^[0-9a-f]{64}$/u.test(declaredDigest)
          || declaredDigest !== expected.sha256
        ) {
          await response.body?.cancel();
          throw new XenoteerError(
            "invalid_response",
            "artifact response integrity headers did not match its reference",
          );
        }
      }
      const declared = response.headers.get("content-length");
      if (
        declared !== null
        && (
          !Number.isSafeInteger(Number(declared))
          || Number(declared) < 0
          || Number(declared) > limit
        )
      ) {
        await response.body?.cancel();
        throw new XenoteerError("response_too_large", `response exceeds ${limit} bytes`);
      }
    } catch (cause) {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      this.state.signal.removeEventListener("abort", onStateAbort);
      const failure = typedFailure(cause);
      logTerminal("failed", {
        status: response.status,
        errorCode: failure.code,
      });
      throw failure;
    }
    const body = response.body;
    const stateSignal = this.state.signal;
    let streamBytes = 0;
    let streamReachedEof = false;
    const bytes = (async function* (): AsyncGenerator<Uint8Array> {
      const reader = body?.getReader();
      try {
        if (reader === undefined) {
          streamReachedEof = true;
          return;
        }
        while (true) {
          let next: ReadableStreamReadResult<Uint8Array>;
          try {
            next = await reader.read();
          } catch (cause) {
            const failure = typedFailure(cause);
            logTerminal("failed", {
              status: response.status,
              errorCode: failure.code,
            });
            throw failure;
          }
          if (next.done) {
            streamReachedEof = true;
            return;
          }
          streamBytes += next.value.byteLength;
          if (streamBytes > limit) {
            await reader.cancel();
            const failure = new XenoteerError(
              "response_too_large",
              `response exceeds ${limit} bytes`,
            );
            logTerminal("failed", {
              status: response.status,
              errorCode: failure.code,
            });
            throw failure;
          }
          yield next.value;
        }
      } finally {
        if (!streamReachedEof && !terminalLogged) {
          try {
            await reader?.cancel();
          } catch {
            // The safe terminal event is authoritative even if cancellation fails.
          }
          logTerminal("failed", {
            status: response.status,
            responseBytes: streamBytes,
            errorCode: "request_cancelled",
          });
        }
        clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
        stateSignal.removeEventListener("abort", onStateAbort);
        reader?.releaseLock();
      }
    })();
    return {
      bytes,
      contentType: contentType(response),
      status: response.status,
      headers: new Headers(response.headers),
      markVerifiedEof: () => {
        if (!streamReachedEof) {
          logTerminal("failed", {
            status: response.status,
            responseBytes: streamBytes,
            errorCode: "invalid_response",
          });
          return;
        }
        logTerminal("succeeded", {
          status: response.status,
          responseBytes: streamBytes,
        });
      },
      markFailed: (errorCode) => {
        logTerminal("failed", {
          status: response.status,
          responseBytes: streamBytes,
          errorCode,
        });
      },
      abortBeforeRead: async (errorCode) => {
        try {
          await body?.cancel();
        } catch {
          // The diagnostic outcome remains stable even if cancellation fails.
        }
        clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
        this.state.signal.removeEventListener("abort", onStateAbort);
        logTerminal("failed", {
          status: response.status,
          responseBytes: 0,
          errorCode,
        });
      },
    };
  }

  async deleteEmpty(path: string, options: RequestOptions = {}): Promise<void> {
    return await this.#perform<void>(
      "artifact.delete",
      "DELETE",
      path,
      undefined,
      mergeRequestHeaders({ accept: "application/problem+json" }, options.headers),
      options,
      4096,
      (_response, bytes) => {
        if (bytes.byteLength !== 0) {
          throw new XenoteerError(
            "invalid_response",
            "delete response unexpectedly contained a body",
          );
        }
      },
    );
  }

  async #perform<T>(
    operation: SafeLogOperation,
    method: "GET" | "POST" | "DELETE",
    path: string,
    body: BodyInit | undefined,
    extraHeaders: Readonly<Record<string, string>>,
    options: RequestOptions,
    maxResponseBytes: number,
    decode: (response: Response, bytes: Uint8Array) => T | Promise<T>,
  ): Promise<T> {
    this.state.assertOpen();
    validatePath(path);
    const timeoutMs = validatePositiveInteger(
      options.timeoutMs ?? this.#timeoutMs,
      3_600_000,
      "request timeout",
    );
    validatePositiveInteger(maxResponseBytes, MAX_ARTIFACT_BYTES, "response byte limit");
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(
      new XenoteerError("request_timeout", "Xenoteer request timed out"),
    ), timeoutMs);
    const onAbort = (): void => controller.abort(
      new XenoteerError(
        "request_cancelled",
        "local request cancelled; server command was not cancelled",
        { cause: options.signal?.reason },
      ),
    );
    const onStateAbort = (): void => controller.abort(this.state.signal.reason);
    if (options.signal?.aborted === true) onAbort();
    else options.signal?.addEventListener("abort", onAbort, { once: true });
    if (this.state.signal.aborted) onStateAbort();
    else this.state.signal.addEventListener("abort", onStateAbort, { once: true });
    try {
      const requestBytes = body === undefined
        ? undefined
        : typeof body === "string"
          ? new TextEncoder().encode(body).byteLength
          : body instanceof ArrayBuffer
            ? body.byteLength
            : undefined;
      const route = classifyRoute(path);
      this.safeLog({
        operation,
        outcome: "started",
        attempt: 1,
        method,
        route,
        ...(requestBytes === undefined ? {} : { requestBytes }),
      });
      const headers = new Headers({
        authorization: await this.authorizationHeader(controller.signal),
        ...extraHeaders,
      });
      const init = {
        method,
        headers,
        signal: controller.signal,
        ...(body === undefined ? {} : { body }),
        ...(body instanceof ReadableStream ? { duplex: "half" } : {}),
      } as RequestInit;
      let response: Response;
      try {
        response = await this.#fetch(`${this.#baseUrl}${path}`, init);
      } catch (cause) {
        if (controller.signal.reason instanceof XenoteerError) {
          throw controller.signal.reason;
        }
        if (controller.signal.aborted) {
          throw new XenoteerError("request_timeout", "Xenoteer request timed out", { cause });
        }
        throw new XenoteerError("transport", "Xenoteer transport failed", { cause });
      }
      const bytes = await collectBounded(response, maxResponseBytes);
      if (!response.ok) {
        if (contentType(response) === "application/problem+json") {
          const decoded = decodeJson(bytes);
          if (typeof decoded === "object" && decoded !== null && !Array.isArray(decoded)) {
            throw errorFromProblem(response.status, decoded as ApiProblem);
          }
          throw new XenoteerError("invalid_response", "server returned an invalid problem document");
        }
        throw new XenoteerError(
          "unexpected_http_status",
          `Xenoteer request failed with HTTP ${response.status}`,
          { status: response.status },
        );
      }
      const decoded = await decode(response, bytes);
      this.safeLog({
        operation,
        outcome: "succeeded",
        attempt: 1,
        method,
        route,
        status: response.status,
        ...(requestBytes === undefined ? {} : { requestBytes }),
        responseBytes: bytes.byteLength,
      });
      return decoded;
    } catch (error) {
      const failure = error instanceof XenoteerError
        ? error
        : new XenoteerError("transport", "Xenoteer transport failed", {
            cause: error,
          });
      this.safeLog({
        operation,
        outcome: "failed",
        attempt: 1,
        method,
        route: classifyRoute(path),
        errorCode: failure.code,
        ...(failure.status === undefined ? {} : { status: failure.status }),
      });
      throw failure;
    } finally {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      this.state.signal.removeEventListener("abort", onStateAbort);
    }
  }
}
