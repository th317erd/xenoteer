// SPDX-License-Identifier: Apache-2.0

import { createHash } from "node:crypto";

import { CommandHandle } from "./command.js";
import type { Desktop, SubmitOptions } from "./desktop.js";
import { XenoteerError } from "./errors.js";
import type {
  ArtifactRef,
  ClipboardReadRequest,
  ClipboardReadResult,
  JsonObject,
  ProcessRef,
  ScreenshotRequest,
  ScreenshotResult,
  SelectionName,
  ViewerTicket,
} from "./protocol.generated.js";
import type { HttpTransport, RequestOptions } from "./transport.js";
import { asCanonicalUInt64 } from "./wire.js";

const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;
const SHA256 = /^[0-9a-f]{64}$/u;
const ARTIFACT_PURPOSES = new Set([
  "clipboard_input",
  "clipboard_output",
  "screenshot",
  "action_trace",
  "support_bundle",
]);

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validateTimestamp(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !Number.isFinite(Date.parse(value))) {
    throw new XenoteerError("invalid_response", `${label} is not a timestamp`);
  }
}

function validateUuid(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !UUID.test(value) || /^0{8}-0{4}-0{4}-0{4}-0{12}$/u.test(value)) {
    throw new XenoteerError("invalid_response", `${label} is not a non-nil UUID`);
  }
}

function validateScope(
  value: Record<string, unknown>,
  desktop: Desktop,
): void {
  if (
    value["desktop_id"] !== desktop.id
    || value["desktop_generation"] !== desktop.generation
  ) {
    throw new XenoteerError("stale_reference", "response belongs to another desktop generation");
  }
}

export function validateArtifactRef(
  value: unknown,
  desktop: Desktop,
  expectedPurpose?: ArtifactRef["purpose"],
): ArtifactRef {
  if (
    !isObject(value)
    || typeof value["purpose"] !== "string"
    || !ARTIFACT_PURPOSES.has(value["purpose"])
    || typeof value["content_type"] !== "string"
    || value["content_type"].length < 1
    || value["content_type"].length > 255
    || !Number.isSafeInteger(value["content_length"])
    || (value["content_length"] as number) < 1
    || (value["content_length"] as number) > 33_554_432
    || typeof value["sha256"] !== "string"
    || !SHA256.test(value["sha256"])
  ) {
    throw new XenoteerError("invalid_response", "server returned an invalid artifact reference");
  }
  validateUuid(value["artifact_id"], "artifact ID");
  validateUuid(value["desktop_id"], "artifact desktop ID");
  validateUuid(value["desktop_generation"], "artifact desktop generation");
  validateTimestamp(value["created_at"], "artifact creation time");
  validateTimestamp(value["expires_at"], "artifact expiry time");
  validateScope(value, desktop);
  if (expectedPurpose !== undefined && value["purpose"] !== expectedPurpose) {
    throw new XenoteerError("invalid_response", "artifact purpose does not match the operation");
  }
  return Object.freeze(structuredClone(value)) as unknown as ArtifactRef;
}

/** Removes every additive response field before an authority-bearing request. */
export function artifactCommandRef(artifact: ArtifactRef): JsonObject {
  return {
    artifact_id: artifact.artifact_id,
    purpose: artifact.purpose,
    desktop_id: artifact.desktop_id,
    desktop_generation: artifact.desktop_generation,
    content_type: artifact.content_type,
    content_length: artifact.content_length,
    sha256: artifact.sha256,
    created_at: artifact.created_at,
    expires_at: artifact.expires_at,
  };
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const copy = new Uint8Array(bytes);
  const digest = await globalThis.crypto.subtle.digest(
    "SHA-256",
    copy.buffer,
  );
  return [...new Uint8Array(digest)]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}

export class Artifact {
  readonly ref: ArtifactRef;
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;

  constructor(desktop: Desktop, transport: HttpTransport, ref: ArtifactRef) {
    this.#desktop = desktop;
    this.#transport = transport;
    this.ref = validateArtifactRef(ref, desktop);
  }

  async download(
    options: RequestOptions & { readonly maxBytes?: number } = {},
  ): Promise<Uint8Array> {
    const chunks: Uint8Array[] = [];
    let total = 0;
    for await (const chunk of this.stream(options)) {
      chunks.push(chunk);
      total += chunk.byteLength;
    }
    const result = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      result.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return result;
  }

  async *stream(
    options: RequestOptions & { readonly maxBytes?: number } = {},
  ): AsyncGenerator<Uint8Array> {
    const maximum = options.maxBytes ?? this.ref.content_length;
    if (!Number.isSafeInteger(maximum) || maximum < 1 || maximum > this.#transport.maxArtifactBytes) {
      throw new XenoteerError("invalid_request", "artifact download bound is invalid");
    }
    if (this.ref.content_length > maximum) {
      throw new XenoteerError("response_too_large", "artifact exceeds the caller download bound");
    }
    const query = new URLSearchParams({
      desktop_id: this.#desktop.id,
      desktop_generation: this.#desktop.generation,
    });
    const result = await this.#transport.downloadStream(
      `/v1/artifacts/${encodeURIComponent(this.ref.artifact_id)}?${query.toString()}`,
      { ...options, maxResponseBytes: maximum },
      {
        contentLength: this.ref.content_length,
        sha256: this.ref.sha256,
      },
    );
    if (result.contentType !== this.ref.content_type) {
      await result.abortBeforeRead("invalid_response");
      throw new XenoteerError("invalid_response", "artifact content type did not match its reference");
    }
    const digest = createHash("sha256");
    let length = 0;
    let verified = false;
    try {
      for await (const chunk of result.bytes) {
        length += chunk.byteLength;
        digest.update(chunk);
        yield chunk;
      }
      if (length !== this.ref.content_length) {
        throw new XenoteerError(
          "invalid_response",
          "artifact length did not match its reference",
        );
      }
      if (digest.digest("hex") !== this.ref.sha256) {
        throw new XenoteerError(
          "invalid_response",
          "artifact digest did not match its reference",
        );
      }
      result.markVerifiedEof();
      verified = true;
    } catch (cause) {
      const failure = cause instanceof XenoteerError
        ? cause
        : new XenoteerError(
            "transport",
            "artifact response stream failed",
            { cause },
          );
      result.markFailed(failure.code);
      throw failure;
    } finally {
      if (!verified) result.markFailed("request_cancelled");
    }
  }

  async delete(options: RequestOptions = {}): Promise<void> {
    const query = new URLSearchParams({
      desktop_id: this.#desktop.id,
      desktop_generation: this.#desktop.generation,
    });
    await this.#transport.deleteEmpty(
      `/v1/artifacts/${encodeURIComponent(this.ref.artifact_id)}?${query.toString()}`,
      options,
    );
  }

  toString(): string {
    return `Artifact(purpose=${this.ref.purpose}, bytes=${this.ref.content_length}, id=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({
      purpose: this.ref.purpose,
      contentType: this.ref.content_type,
      contentLength: this.ref.content_length,
      sha256: this.ref.sha256,
      artifactId: "<redacted>",
    });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}

export class Artifacts {
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;

  constructor(desktop: Desktop, transport: HttpTransport) {
    this.#desktop = desktop;
    this.#transport = transport;
  }

  async uploadClipboardInput(
    input: Uint8Array,
    contentType = "application/octet-stream",
    options: RequestOptions = {},
  ): Promise<Artifact> {
    if (
      input.byteLength < 1
      || input.byteLength > 1_048_576
      || !/^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+(?:; charset=utf-8)?$/u.test(contentType)
    ) {
      throw new XenoteerError("invalid_request", "clipboard artifact upload is invalid");
    }
    const bytes = new Uint8Array(input);
    const digest = await sha256Hex(bytes);
    const ref = await this.#transport.upload<ArtifactRef>(
      "/v1/artifacts?purpose=clipboard_input",
      bytes,
      contentType,
      {
        ...options,
        maxRequestBytes: 1_048_576,
        maxResponseBytes: 64 * 1024,
      },
      (response) => {
        const uploaded = validateArtifactRef(
          response,
          this.#desktop,
          "clipboard_input",
        );
        if (
          uploaded.content_length !== bytes.byteLength
          || uploaded.sha256 !== digest
          || uploaded.content_type !== contentType
        ) {
          throw new XenoteerError(
            "invalid_response",
            "artifact upload metadata did not match input",
          );
        }
        return uploaded;
      },
      digest,
    );
    return new Artifact(this.#desktop, this.#transport, ref);
  }

  async uploadClipboardInputStream(
    source: AsyncIterable<Uint8Array>,
    metadata: {
      readonly contentLength: number;
      readonly sha256: string;
      readonly contentType?: string;
    },
    options: RequestOptions = {},
  ): Promise<Artifact> {
    const contentType = metadata.contentType ?? "application/octet-stream";
    if (
      !Number.isSafeInteger(metadata.contentLength)
      || metadata.contentLength < 1
      || metadata.contentLength > 1_048_576
      || !SHA256.test(metadata.sha256)
      || !/^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+(?:; charset=utf-8)?$/u.test(contentType)
    ) {
      throw new XenoteerError("invalid_request", "clipboard artifact stream metadata is invalid");
    }
    const iterator = source[Symbol.asyncIterator]();
    const digest = createHash("sha256");
    let transferred = 0;
    const stream = new ReadableStream<Uint8Array>({
      pull: async (controller) => {
        const next = await iterator.next();
        if (next.done) {
          controller.close();
          return;
        }
        if (!(next.value instanceof Uint8Array) || next.value.byteLength < 1) {
          controller.error(new XenoteerError("invalid_request", "artifact source yielded an invalid chunk"));
          return;
        }
        transferred += next.value.byteLength;
        if (transferred > metadata.contentLength) {
          controller.error(new XenoteerError("request_too_large", "artifact source exceeded declared length"));
          return;
        }
        digest.update(next.value);
        controller.enqueue(next.value);
      },
      cancel: async () => {
        await iterator.return?.();
      },
    });
    const ref = await this.#transport.uploadStream<ArtifactRef>(
      "/v1/artifacts?purpose=clipboard_input",
      stream,
      metadata.contentLength,
      contentType,
      {
        ...options,
        maxRequestBytes: 1_048_576,
        maxResponseBytes: 64 * 1024,
      },
      (response) => {
        if (
          transferred !== metadata.contentLength
          || digest.digest("hex") !== metadata.sha256
        ) {
          throw new XenoteerError(
            "invalid_response",
            "artifact source did not match its declared length or digest",
          );
        }
        const uploaded = validateArtifactRef(
          response,
          this.#desktop,
          "clipboard_input",
        );
        if (
          uploaded.content_length !== metadata.contentLength
          || uploaded.sha256 !== metadata.sha256
          || uploaded.content_type !== contentType
        ) {
          throw new XenoteerError(
            "invalid_response",
            "artifact upload metadata did not match input",
          );
        }
        return uploaded;
      },
      metadata.sha256,
    );
    return new Artifact(this.#desktop, this.#transport, ref);
  }

  fromRef(ref: ArtifactRef): Artifact {
    return new Artifact(this.#desktop, this.#transport, ref);
  }
}

export class ClipboardValue {
  readonly result: ClipboardReadResult;
  readonly artifact?: Artifact;

  constructor(result: ClipboardReadResult, artifact?: Artifact) {
    this.result = result;
    if (artifact !== undefined) this.artifact = artifact;
  }

  text(): string {
    const content = this.result.content;
    if (!isObject(content) || content["delivery"] !== "inline_text" || typeof content["text"] !== "string") {
      throw new XenoteerError("invalid_response", "clipboard value is not inline text");
    }
    return content["text"];
  }

  toString(): string {
    const content = this.result.content;
    const delivery = isObject(content) && typeof content["delivery"] === "string"
      ? content["delivery"]
      : "unknown";
    return `ClipboardValue(selection=${this.result.selection}, delivery=${delivery}, content=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({
      selection: this.result.selection,
      revision: this.result.revision,
      content: "<redacted>",
    });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}

export class Clipboard {
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;

  constructor(desktop: Desktop, transport: HttpTransport) {
    this.#desktop = desktop;
    this.#transport = transport;
  }

  async read(
    request: Partial<ClipboardReadRequest> = {},
    options: RequestOptions = {},
  ): Promise<ClipboardValue> {
    const body: ClipboardReadRequest = {
      selection: request.selection ?? "clipboard",
      preferred_targets: request.preferred_targets ?? [],
      allow_binary_fallback: request.allow_binary_fallback ?? false,
    };
    if (
      !["clipboard", "primary"].includes(body.selection)
      || body.preferred_targets.length > 32
      || body.preferred_targets.some((target) => typeof target !== "string" || target.length > 255)
    ) {
      throw new XenoteerError("invalid_request", "clipboard read request is invalid");
    }
    const value = await this.#transport.request<ClipboardReadResult>(
      "POST",
      `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/clipboard/read`,
      body,
      { ...options, maxRequestBytes: 16 * 1024, maxResponseBytes: 1_048_576 },
    );
    if (
      !isObject(value)
      || value.selection !== body.selection
      || !isObject(value.content)
      || !Object.hasOwn(value, "evidence")
    ) {
      throw new XenoteerError("invalid_response", "clipboard read response is invalid");
    }
    asCanonicalUInt64(value.revision, { allowZero: false });
    const content = value.content;
    let artifact: Artifact | undefined;
    if (content["delivery"] === "inline_text") {
      if (typeof content["text"] !== "string") {
        throw new XenoteerError("invalid_response", "inline clipboard text is invalid");
      }
    } else if (content["delivery"] === "inline_binary") {
      if (typeof content["data"] !== "string") {
        throw new XenoteerError("invalid_response", "inline clipboard binary is invalid");
      }
    } else if (content["delivery"] === "artifact") {
      artifact = new Artifact(
        this.#desktop,
        this.#transport,
        validateArtifactRef(content["artifact"], this.#desktop, "clipboard_output"),
      );
    } else {
      throw new XenoteerError("unsupported_response_variant", "clipboard delivery variant is unsupported");
    }
    return new ClipboardValue(Object.freeze(structuredClone(value)), artifact);
  }

  async setText(
    text: string,
    leaseId: string,
    options: SubmitOptions & { readonly selection?: SelectionName } = {},
  ): Promise<CommandHandle> {
    const bytes = new TextEncoder().encode(text);
    const { selection = "clipboard", ...submit } = options;
    if (bytes.byteLength > 1_048_576) {
      throw new XenoteerError("request_too_large", "clipboard text exceeds 1 MiB");
    }
    if (bytes.byteLength <= 256 * 1024) {
      return await this.#desktop.submit(
        { type: "selection_set", selection, content: { source: "inline_text", text } },
        { ...submit, leaseId },
      );
    }
    const artifact = await new Artifacts(this.#desktop, this.#transport)
      .uploadClipboardInput(bytes, "text/plain; charset=utf-8", submit);
    const handle = await this.#desktop.submit(
      {
        type: "selection_set",
        selection,
        content: {
          source: "artifact",
          target: "text/plain;charset=utf-8",
          artifact: artifactCommandRef(artifact.ref),
        },
      },
      { ...submit, leaseId },
    );
    return await handle.attachTerminalCleanup(async () => await artifact.delete());
  }

  async clear(
    leaseId: string,
    selection: SelectionName = "clipboard",
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    return await this.#desktop.submit(
      { type: "selection_clear", selection },
      { ...options, leaseId },
    );
  }
}

export class Capture {
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;

  constructor(desktop: Desktop, transport: HttpTransport) {
    this.#desktop = desktop;
    this.#transport = transport;
  }

  async screenshot(
    request: ScreenshotRequest,
    options: RequestOptions = {},
  ): Promise<{ readonly result: ScreenshotResult; readonly artifact?: Artifact }> {
    if (
      request.max_bytes !== undefined
      && request.max_bytes !== null
      && (!Number.isSafeInteger(request.max_bytes) || request.max_bytes < 1 || request.max_bytes > 33_554_432)
    ) {
      throw new XenoteerError("invalid_request", "screenshot max_bytes is invalid");
    }
    const value = await this.#transport.request<ScreenshotResult>(
      "POST",
      `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/screenshots`,
      request,
      {
        ...options,
        maxRequestBytes: 64 * 1024,
        maxResponseBytes: 256 * 1024,
      },
    );
    if (
      !isObject(value)
      || typeof value.sha256 !== "string"
      || !SHA256.test(value.sha256)
      || !isObject(value.delivery)
    ) {
      throw new XenoteerError("invalid_response", "screenshot response is invalid");
    }
    let artifact: Artifact | undefined;
    if (value.delivery["delivery"] === "artifact") {
      artifact = new Artifact(
        this.#desktop,
        this.#transport,
        validateArtifactRef(value.delivery["artifact"], this.#desktop, "screenshot"),
      );
      if (artifact.ref.sha256 !== value.sha256) {
        throw new XenoteerError("invalid_response", "screenshot and artifact digests differ");
      }
    } else if (value.delivery["delivery"] !== "inline_body") {
      throw new XenoteerError("unsupported_response_variant", "screenshot delivery variant is unsupported");
    }
    return Object.freeze({ result: Object.freeze(structuredClone(value)), ...(artifact === undefined ? {} : { artifact }) });
  }
}

export class IssuedViewerTicket {
  readonly #origin: string;
  readonly #expiresAt: string;
  readonly #mode: string;
  readonly #audience: ViewerTicket["audience"];
  readonly #usePolicy: ViewerTicket["use_policy"];
  #ticket: string | null;

  constructor(value: ViewerTicket) {
    this.#ticket = value.ticket;
    this.#origin = value.origin;
    this.#expiresAt = value.expires_at;
    this.#mode = value.mode;
    this.#audience = value.audience;
    this.#usePolicy = value.use_policy;
  }

  /** Intentional one-time bearer access for a viewer WebSocket subprotocol only. */
  consumeSecret(): string {
    if (this.#ticket === null) {
      throw new XenoteerError("invalid_request", "viewer ticket secret was already consumed");
    }
    const ticket = this.#ticket;
    this.#ticket = null;
    return ticket;
  }

  get expiresAt(): string {
    return this.#expiresAt;
  }

  get origin(): string {
    return this.#origin;
  }

  get audience(): ViewerTicket["audience"] {
    return this.#audience;
  }

  get usePolicy(): ViewerTicket["use_policy"] {
    return this.#usePolicy;
  }

  toString(): string {
    return `ViewerTicket(origin=${this.#origin}, expiresAt=${this.#expiresAt}, ticket=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({
      origin: this.#origin,
      expiresAt: this.#expiresAt,
      mode: this.#mode,
      audience: this.#audience,
      usePolicy: this.#usePolicy,
      ticket: "<redacted>",
    });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}

export class Viewer {
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;

  constructor(desktop: Desktop, transport: HttpTransport) {
    this.#desktop = desktop;
    this.#transport = transport;
  }

  pageUrl(): string {
    return new URL(
      `/viewer/${encodeURIComponent(this.#desktop.id)}/${encodeURIComponent(this.#desktop.generation)}/`,
      this.#transport.baseUrl,
    ).toString();
  }

  async issueTicket(origin: string, options: RequestOptions = {}): Promise<IssuedViewerTicket> {
    let parsed: URL;
    try {
      parsed = new URL(origin);
    } catch {
      throw new XenoteerError("invalid_request", "viewer origin is invalid");
    }
    if (
      !["http:", "https:"].includes(parsed.protocol)
      || parsed.origin !== origin
      || parsed.username !== ""
      || parsed.password !== ""
    ) {
      throw new XenoteerError("invalid_request", "viewer origin must be an exact HTTP(S) origin");
    }
    const value = await this.#transport.request<ViewerTicket>(
      "POST",
      `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/viewer-tickets`,
      {
        desktop_id: this.#desktop.id,
        desktop_generation: this.#desktop.generation,
        mode: "view_only",
      },
      {
        ...options,
        headers: { ...options.headers, origin },
        maxRequestBytes: 8 * 1024,
        maxResponseBytes: 64 * 1024,
      },
    );
    if (
      !isObject(value)
      || value.desktop_id !== this.#desktop.id
      || value.desktop_generation !== this.#desktop.generation
      || value.origin !== origin
      || value.mode !== "view_only"
      || value.audience !== "viewer_websocket"
      || value.use_policy !== "single_use"
      || typeof value.ticket !== "string"
      || value.ticket.length < 32
      || value.ticket.length > 2048
      || typeof value.principal_id !== "string"
    ) {
      throw new XenoteerError("invalid_response", "viewer ticket response is invalid");
    }
    validateTimestamp(value.issued_at, "viewer ticket issue time");
    validateTimestamp(value.expires_at, "viewer ticket expiry time");
    return new IssuedViewerTicket(Object.freeze(structuredClone(value)));
  }
}

function validateProcessRef(process: ProcessRef, desktop: Desktop): void {
  if (
    process.desktop_generation !== desktop.generation
    || !Number.isSafeInteger(process.pid)
    || process.pid < 1
    || process.pid > 4_294_967_295
  ) {
    throw new XenoteerError("stale_reference", "managed process reference is invalid or stale");
  }
  validateUuid(process.launch_id, "launch ID");
  asCanonicalUInt64(process.proc_start_ticks, { allowZero: false });
}

export class Applications {
  readonly #desktop: Desktop;

  constructor(desktop: Desktop) {
    this.#desktop = desktop;
  }

  async launch(
    application: string,
    args: readonly string[] = [],
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    if (
      !/^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/u.test(application)
      || args.length > 64
      || args.some((arg) => typeof arg !== "string" || new TextEncoder().encode(arg).byteLength > 4096 || arg.includes("\0"))
    ) {
      throw new XenoteerError("invalid_request", "application launch request is invalid");
    }
    return await this.#desktop.submit(
      { type: "application_launch", application, arguments: [...args] },
      options,
    );
  }

  async status(process: ProcessRef, options: SubmitOptions = {}): Promise<CommandHandle> {
    validateProcessRef(process, this.#desktop);
    return await this.#desktop.submit({ type: "process_status", process }, options);
  }

  async terminate(
    process: ProcessRef,
    graceMs: number | null = null,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    validateProcessRef(process, this.#desktop);
    if (graceMs !== null && (!Number.isInteger(graceMs) || graceMs < 0 || graceMs > 30_000)) {
      throw new XenoteerError("invalid_request", "process termination grace is invalid");
    }
    return await this.#desktop.submit(
      { type: "process_terminate", process, grace_ms: graceMs },
      options,
    );
  }
}
