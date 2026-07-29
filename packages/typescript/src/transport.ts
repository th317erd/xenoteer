// SPDX-License-Identifier: Apache-2.0

import { errorFromProblem, XenoteerError } from "./errors.js";
import {
  BearerToken,
  type SafeLogEvent,
  type SafeLogHook,
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
}

export type CloseHandler = () => void | Promise<void>;
export type ReconnectHandler = () => void | Promise<void>;

/** One lifecycle fence shared by the root, desktops, handles, leases, and sessions. */
export class ClientConnectionState {
  #closed = false;
  readonly #handlers = new Set<CloseHandler>();
  readonly #reconnectHandlers = new Set<ReconnectHandler>();
  #desktopId?: string;
  #desktopGeneration: string | undefined;

  get closed(): boolean {
    return this.#closed;
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
    const handlers = [...this.#handlers];
    this.#handlers.clear();
    this.#reconnectHandlers.clear();
    await Promise.allSettled(handlers.map(async (handler) => await handler()));
  }
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

  async authorizationHeader(): Promise<string> {
    this.state.assertOpen();
    let source: string;
    try {
      source = typeof this.#token === "function" ? await this.#token() : this.#token;
    } catch {
      throw new XenoteerError("invalid_token", "Xenoteer token provider failed");
    }
    return new BearerToken(source).authorizationHeader();
  }

  safeLog(event: SafeLogEvent): void {
    if (this.#log === undefined) return;
    try {
      this.#log(Object.freeze({ ...event }));
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
    const headers = {
      accept: "application/json, application/problem+json",
      ...(encoded === undefined ? {} : { "content-type": "application/json" }),
      ...options.headers,
    };
    const response = await this.#perform(
      method,
      path,
      encoded,
      headers,
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
    );
    if (contentType(response.response) !== "application/json") {
      throw new XenoteerError("invalid_response", "successful response was not application/json");
    }
    return decodeJson(response.bytes) as T;
  }

  async upload(
    path: string,
    bytes: Uint8Array,
    contentTypeValue: string,
    options: RequestOptions = {},
  ): Promise<unknown> {
    const max = options.maxRequestBytes ?? this.#maxArtifactBytes;
    if (bytes.byteLength < 1 || bytes.byteLength > max) {
      throw new XenoteerError("request_too_large", `artifact upload must be 1..${max} bytes`);
    }
    const response = await this.#perform(
      "POST",
      path,
      new Uint8Array(bytes).buffer,
      {
        accept: "application/json, application/problem+json",
        "content-type": contentTypeValue,
        "content-length": String(bytes.byteLength),
        ...options.headers,
      },
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
    );
    if (contentType(response.response) !== "application/json") {
      throw new XenoteerError("invalid_response", "artifact upload response was not JSON");
    }
    return decodeJson(response.bytes);
  }

  async uploadStream(
    path: string,
    stream: ReadableStream<Uint8Array>,
    contentLength: number,
    contentTypeValue: string,
    options: RequestOptions = {},
  ): Promise<unknown> {
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
    const response = await this.#perform(
      "POST",
      path,
      stream,
      {
        accept: "application/json, application/problem+json",
        "content-type": contentTypeValue,
        "content-length": String(contentLength),
        ...options.headers,
      },
      options,
      options.maxResponseBytes ?? this.#maxResponseBytes,
    );
    if (contentType(response.response) !== "application/json") {
      throw new XenoteerError("invalid_response", "artifact upload response was not JSON");
    }
    return decodeJson(response.bytes);
  }

  async download(path: string, options: RequestOptions = {}): Promise<ByteResponse> {
    const result = await this.#perform(
      "GET",
      path,
      undefined,
      { accept: "application/octet-stream, image/png, image/webp, image/bmp", ...options.headers },
      options,
      options.maxResponseBytes ?? this.#maxArtifactBytes,
    );
    return {
      bytes: result.bytes,
      contentType: contentType(result.response),
      status: result.response.status,
      headers: new Headers(result.response.headers),
    };
  }

  async downloadStream(
    path: string,
    options: RequestOptions = {},
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
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    const onAbort = (): void => controller.abort(options.signal?.reason);
    if (options.signal?.aborted === true) controller.abort(options.signal.reason);
    else options.signal?.addEventListener("abort", onAbort, { once: true });
    let response: Response;
    try {
      response = await this.#fetch(`${this.#baseUrl}${path}`, {
        method: "GET",
        headers: new Headers({
          authorization: await this.authorizationHeader(),
          accept: "application/octet-stream, image/png, image/webp, image/bmp",
          ...options.headers,
        }),
        signal: controller.signal,
      });
    } catch (cause) {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      if (options.signal?.aborted) {
        throw new XenoteerError(
          "request_cancelled",
          "local artifact download cancelled",
          { cause },
        );
      }
      if (controller.signal.aborted) {
        throw new XenoteerError("request_timeout", "artifact download timed out", { cause });
      }
      throw new XenoteerError("transport", "artifact download transport failed", { cause });
    }
    if (!response.ok) {
      const bytes = await collectBounded(response, Math.min(limit, 64 * 1024));
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
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
    const declared = response.headers.get("content-length");
    if (
      declared !== null
      && (
        !Number.isSafeInteger(Number(declared))
        || Number(declared) < 0
        || Number(declared) > limit
      )
    ) {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      await response.body?.cancel();
      throw new XenoteerError("response_too_large", `response exceeds ${limit} bytes`);
    }
    const body = response.body;
    const bytes = (async function* (): AsyncGenerator<Uint8Array> {
      let total = 0;
      const reader = body?.getReader();
      try {
        if (reader === undefined) return;
        while (true) {
          const next = await reader.read();
          if (next.done) return;
          total += next.value.byteLength;
          if (total > limit) {
            await reader.cancel();
            throw new XenoteerError("response_too_large", `response exceeds ${limit} bytes`);
          }
          yield next.value;
        }
      } finally {
        clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
        reader?.releaseLock();
      }
    })();
    return {
      bytes,
      contentType: contentType(response),
      status: response.status,
      headers: new Headers(response.headers),
    };
  }

  async deleteEmpty(path: string, options: RequestOptions = {}): Promise<void> {
    const result = await this.#perform(
      "DELETE",
      path,
      undefined,
      { accept: "application/problem+json", ...options.headers },
      options,
      4096,
    );
    if (result.bytes.byteLength !== 0) {
      throw new XenoteerError("invalid_response", "delete response unexpectedly contained a body");
    }
  }

  async #perform(
    method: "GET" | "POST" | "DELETE",
    path: string,
    body: BodyInit | undefined,
    extraHeaders: Readonly<Record<string, string>>,
    options: RequestOptions,
    maxResponseBytes: number,
  ): Promise<{ readonly response: Response; readonly bytes: Uint8Array }> {
    this.state.assertOpen();
    validatePath(path);
    const timeoutMs = validatePositiveInteger(
      options.timeoutMs ?? this.#timeoutMs,
      3_600_000,
      "request timeout",
    );
    validatePositiveInteger(maxResponseBytes, MAX_ARTIFACT_BYTES, "response byte limit");
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    const onAbort = (): void => controller.abort(options.signal?.reason);
    if (options.signal?.aborted === true) controller.abort(options.signal.reason);
    else options.signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const requestBytes = body === undefined
        ? undefined
        : typeof body === "string"
          ? new TextEncoder().encode(body).byteLength
          : body instanceof ArrayBuffer
            ? body.byteLength
            : undefined;
      this.safeLog({
        operation: "http.request",
        outcome: "started",
        method,
        path,
        ...(requestBytes === undefined ? {} : { requestBytes }),
      });
      const headers = new Headers({
        authorization: await this.authorizationHeader(),
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
        if (options.signal?.aborted) {
          throw new XenoteerError(
            "request_cancelled",
            "local request cancelled; server command was not cancelled",
            { cause },
          );
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
      this.safeLog({
        operation: "http.request",
        outcome: "succeeded",
        method,
        path,
        status: response.status,
        responseBytes: bytes.byteLength,
      });
      return { response, bytes };
    } catch (error) {
      this.safeLog({
        operation: "http.request",
        outcome: "failed",
        method,
        path,
        ...(error instanceof XenoteerError
          ? {
              errorCode: error.code,
              status: error.status,
            }
          : {}),
      });
      throw error;
    } finally {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
    }
  }
}
