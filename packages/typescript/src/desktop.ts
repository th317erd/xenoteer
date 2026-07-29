// SPDX-License-Identifier: Apache-2.0

import { CommandHandle, CommandSubmission } from "./command.js";
import { validateCommandEnvelopeInput } from "./compatibility.js";
import { XenoteerError } from "./errors.js";
import { ControlLease } from "./lease.js";
import {
  Applications,
  Artifacts,
  Capture,
  Clipboard,
  Viewer,
} from "./domains.js";
import {
  ElementHandle,
  WindowHandle,
  elementHandleFromResolve,
  windowHandleFromResolve,
} from "./handles.js";
import type {
  CommandEnvelope,
  ElementResolveResult,
  ElementSnapshotResult,
  ElementQueryPage,
  ElementQueryRequest,
  ElementWaitRequest,
  ElementWaitResult,
  JsonObject,
  KeyboardKeyIdentifier,
  ProtocolVersion,
  WindowListPage,
  WindowQueryPage,
  WindowQueryRequest,
  WindowResolveResult,
  WindowSnapshotResult,
  WindowWaitRequest,
  WindowWaitResult,
  WireCommand,
} from "./protocol.generated.js";
import type { HttpTransport, RequestOptions } from "./transport.js";
import { asCanonicalUInt64 } from "./wire.js";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;

function requestId(): string {
  return globalThis.crypto.randomUUID();
}

function validateUuid(value: string, label: string): void {
  if (!UUID.test(value) || value === "00000000-0000-0000-0000-000000000000") {
    throw new XenoteerError("invalid_request", `${label} must be a non-nil UUID`);
  }
}

function validateScope(
  value: { readonly desktop_id: string; readonly desktop_generation: string },
  desktop: Desktop,
): void {
  if (value.desktop_id !== desktop.id || value.desktop_generation !== desktop.generation) {
    throw new XenoteerError("invalid_response", "response belongs to another desktop generation");
  }
}

export interface SubmitOptions extends RequestOptions {
  readonly commandId?: string;
  readonly leaseId?: string;
  readonly deadline?: string | null;
  readonly tracePolicy?: "none" | "normal" | "detailed" | null;
}

export type WindowQuerySpec = Omit<
  WindowQueryRequest,
  "desktop_id" | "desktop_generation"
>;
export type WindowWaitSpec = Omit<
  WindowWaitRequest,
  "desktop_id" | "desktop_generation"
>;
export type ElementQuerySpec = Omit<
  ElementQueryRequest,
  "desktop_id" | "desktop_generation"
>;
export type ElementWaitSpec = Omit<
  ElementWaitRequest,
  "desktop_id" | "desktop_generation"
>;
export interface WindowListSpec {
  readonly limit?: number;
  readonly order?: string;
  readonly cursor?: string | null;
}
export interface WindowResolveSpec {
  readonly selector: JsonObject;
  readonly order: string;
  readonly match_policy: string;
}
export interface ElementListSpec {
  readonly scope: JsonObject;
  readonly order: string;
  readonly limit?: number;
  readonly cursor?: string | null;
  readonly limits?: import("./protocol.generated.js").AccessibilityLimits;
  readonly expansion?: JsonObject;
}
export interface ElementResolveSpec {
  readonly selector: JsonObject;
  readonly limits?: import("./protocol.generated.js").AccessibilityLimits;
  readonly expansion?: JsonObject;
}

/** Cheap immutable handle fenced to exactly one desktop lifetime. */
export class Desktop {
  readonly id: string;
  readonly generation: string;
  readonly protocol: ProtocolVersion;
  readonly #transport: HttpTransport;

  constructor(
    transport: HttpTransport,
    id: string,
    generation: string,
    protocol: ProtocolVersion,
  ) {
    validateUuid(id, "desktop id");
    validateUuid(generation, "desktop generation");
    this.#transport = transport;
    this.id = id;
    this.generation = generation;
    this.protocol = protocol;
  }

  get clipboard(): Clipboard {
    this.assertCurrentReference();
    return new Clipboard(this, this.#transport);
  }

  get artifacts(): Artifacts {
    this.assertCurrentReference();
    return new Artifacts(this, this.#transport);
  }

  get capture(): Capture {
    this.assertCurrentReference();
    return new Capture(this, this.#transport);
  }

  get viewer(): Viewer {
    this.assertCurrentReference();
    return new Viewer(this, this.#transport);
  }

  get applications(): Applications {
    this.assertCurrentReference();
    return new Applications(this);
  }

  prepareSubmission(
    command: WireCommand,
    options: SubmitOptions = {},
  ): CommandSubmission {
    this.#transport.state.assertGeneration(this.id, this.generation, "command");
    const commandId = options.commandId ?? requestId();
    validateUuid(commandId, "command id");
    if (options.leaseId !== undefined) validateUuid(options.leaseId, "lease id");
    if (typeof command.type !== "string" || command.type.length === 0) {
      throw new XenoteerError("invalid_request", "command type is required");
    }
    if (
      options.deadline !== undefined
      && options.deadline !== null
      && !Number.isFinite(Date.parse(options.deadline))
    ) {
      throw new XenoteerError("invalid_request", "deadline must be an RFC 3339 timestamp");
    }
    const envelope: CommandEnvelope = {
      protocol_version: this.protocol,
      request_id: requestId(),
      command_id: commandId,
      desktop_id: this.id,
      desktop_generation: this.generation,
      lease_id: options.leaseId ?? null,
      deadline: options.deadline ?? null,
      trace_policy: options.tracePolicy ?? null,
      command,
    };
    return new CommandSubmission(
      this.#transport,
      validateCommandEnvelopeInput(envelope),
    );
  }

  async submit(
    command: WireCommand,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    const submission = this.prepareSubmission(command, options);
    return await submission.send(
      options.signal === undefined ? {} : { signal: options.signal },
    );
  }

  /** Reattaches to an already accepted ledger entry without replaying its mutation. */
  async command(
    commandId: string,
    options: RequestOptions = {},
  ): Promise<CommandHandle> {
    this.#transport.state.assertGeneration(this.id, this.generation, "command");
    validateUuid(commandId, "command id");
    const result = await this.#transport.request<import("./protocol.generated.js").CommandResult>(
      "GET",
      `/v1/desktops/${encodeURIComponent(this.id)}/commands/${encodeURIComponent(commandId)}`,
      undefined,
      { ...options, maxResponseBytes: 1_048_576 },
    );
    return new CommandHandle(
      this.#transport,
      this.id,
      this.generation,
      result,
    );
  }

  async acquireControl(
    ttlMs?: number,
    options: RequestOptions = {},
  ): Promise<ControlLease> {
    this.#transport.state.assertGeneration(this.id, this.generation, "lease");
    if (
      ttlMs !== undefined
      && (!Number.isInteger(ttlMs) || ttlMs < 1 || ttlMs > 3_600_000)
    ) {
      throw new XenoteerError("invalid_request", "lease TTL must be 1..3600000ms");
    }
    const body = {
      protocol_version: this.protocol,
      request_id: requestId(),
      desktop_id: this.id,
      desktop_generation: this.generation,
      ...(ttlMs === undefined ? {} : { ttl_ms: ttlMs }),
    };
    const state = await this.#transport.request<import("./protocol.generated.js").LeaseState>(
      "POST",
      `/v1/desktops/${encodeURIComponent(this.id)}/lease`,
      body,
      options,
    );
    return new ControlLease(this, this.#transport, state, ttlMs);
  }

  /** Explicit scope helper; it always awaits release after the callback settles. */
  async withControl<T>(
    ttlMs: number | undefined,
    callback: (lease: ControlLease) => Promise<T>,
  ): Promise<T> {
    const lease = await this.acquireControl(ttlMs);
    lease.startAutoRenewal();
    let result: T | undefined;
    let callbackFailure: unknown;
    try {
      result = await callback(lease);
    } catch (error) {
      callbackFailure = error;
    }
    lease.stopAutoRenewal();
    try {
      if (lease.active) await lease.release();
    } catch (releaseFailure) {
      if (callbackFailure === undefined) throw releaseFailure;
    }
    if (callbackFailure !== undefined) throw callbackFailure;
    return result as T;
  }

  readonly windows = {
    one: async (
      selector: JsonObject,
      order: string,
      options: RequestOptions = {},
    ): Promise<WindowHandle> => {
      const spec: WindowResolveSpec = {
        selector,
        order,
        match_policy: "exactly_one",
      };
      const response = await this.windows.resolve(spec, options);
      return windowHandleFromResolve(this, selector, order, spec, response.window);
    },
    first: async (
      selector: JsonObject,
      order: string,
      options: RequestOptions = {},
    ): Promise<WindowHandle> => {
      if (order.length === 0) {
        throw new XenoteerError("invalid_request", "window first() requires explicit ordering");
      }
      const spec: WindowResolveSpec = {
        selector,
        order,
        match_policy: "first",
      };
      const response = await this.windows.resolve(spec, options);
      return windowHandleFromResolve(this, selector, order, spec, response.window);
    },
    list: async (
      spec: WindowListSpec = {},
      options: RequestOptions = {},
    ): Promise<WindowListPage> => {
      if (
        spec.limit !== undefined
        && (!Number.isInteger(spec.limit) || spec.limit < 1 || spec.limit > 200)
      ) {
        throw new XenoteerError("invalid_request", "window list limit must be 1..200");
      }
      const query = new URLSearchParams({
        desktop_generation: this.generation,
        ...(spec.limit === undefined ? {} : { limit: String(spec.limit) }),
        ...(spec.order === undefined ? {} : { order: spec.order }),
        ...(spec.cursor === undefined || spec.cursor === null ? {} : { cursor: spec.cursor }),
      });
      const response = await this.#transport.request<WindowListPage>(
        "GET",
        `/v1/desktops/${encodeURIComponent(this.id)}/windows?${query.toString()}`,
        undefined,
        { ...options, maxResponseBytes: 4 * 1_048_576 },
      );
      validateWindowPage(response, this);
      return response;
    },
    query: async (
      spec: WindowQuerySpec,
      options: RequestOptions = {},
    ): Promise<WindowQueryPage> => {
      const request: WindowQueryRequest = {
        desktop_id: this.id,
        desktop_generation: this.generation,
        ...spec,
      };
      const response = await this.#transport.request<WindowQueryPage>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/windows/query`,
        request,
        { ...options, maxRequestBytes: 256 * 1024, maxResponseBytes: 4 * 1_048_576 },
      );
      validateWindowPage(response, this);
      return response;
    },
    resolve: async (
      spec: WindowResolveSpec,
      options: RequestOptions = {},
    ): Promise<WindowResolveResult> => {
      const response = await this.#transport.request<WindowResolveResult>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/windows/resolve`,
        { desktop_id: this.id, desktop_generation: this.generation, ...spec },
        { ...options, maxRequestBytes: 256 * 1024, maxResponseBytes: 1_048_576 },
      );
      validateScope(response, this);
      asCanonicalUInt64(response.snapshot_revision);
      validateWindowEntry(response.window, this);
      return response;
    },
    snapshot: async (
      referenceToken: string,
      options: RequestOptions = {},
    ): Promise<WindowSnapshotResult> => {
      if (!/^[A-Za-z0-9_-]{16,2048}$/u.test(referenceToken)) {
        throw new XenoteerError("invalid_request", "window reference token is invalid");
      }
      const query = new URLSearchParams({ desktop_generation: this.generation });
      const response = await this.#transport.request<WindowSnapshotResult>(
        "GET",
        `/v1/desktops/${encodeURIComponent(this.id)}/windows/${encodeURIComponent(referenceToken)}?${query.toString()}`,
        undefined,
        { ...options, maxResponseBytes: 1_048_576 },
      );
      asCanonicalUInt64(response.snapshot_revision);
      validateWindowEntry(response.window, this);
      return response;
    },
    wait: async (
      spec: WindowWaitSpec,
      options: RequestOptions = {},
    ): Promise<WindowWaitResult> => {
      if (!Number.isInteger(spec.timeout_ms) || spec.timeout_ms < 1 || spec.timeout_ms > 300_000) {
        throw new XenoteerError("invalid_request", "window wait timeout must be 1..300000ms");
      }
      const request: WindowWaitRequest = {
        desktop_id: this.id,
        desktop_generation: this.generation,
        ...spec,
      };
      const response = await this.#transport.request<WindowWaitResult>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/windows/wait`,
        request,
        { ...options, maxRequestBytes: 256 * 1024, maxResponseBytes: 4 * 1_048_576, timeoutMs: spec.timeout_ms + 5_000 },
      );
      validateScope(response, this);
      asCanonicalUInt64(response.evaluated_revision);
      return response;
    },
  };

  readonly accessibility = {
    one: async (
      selector: JsonObject,
      spec: Omit<ElementResolveSpec, "selector"> = {},
      options: RequestOptions = {},
    ): Promise<ElementHandle> => {
      const request: ElementResolveSpec = { selector, ...spec };
      const response = await this.accessibility.resolve(request, options);
      return elementHandleFromResolve(this, selector, request, response.element);
    },
    first: async (
      selector: JsonObject,
      options: RequestOptions = {},
    ): Promise<ElementHandle> => {
      if (typeof selector["order"] !== "string") {
        throw new XenoteerError("invalid_request", "element first() requires explicit selector ordering");
      }
      const indexed = { ...selector, result_index: 0 };
      const response = await this.accessibility.query(
        { selector: indexed, limit: 1 },
        options,
      );
      const first = response.elements[0];
      if (first === undefined || typeof first !== "object" || first === null || Array.isArray(first)) {
        throw new XenoteerError("unexpected_http_status", "element selector matched no elements", {
          status: 404,
        });
      }
      return new ElementHandle(this, selector, first as JsonObject);
    },
    list: async (
      spec: ElementListSpec,
      options: RequestOptions = {},
    ): Promise<ElementQueryPage> => {
      const response = await this.#transport.request<ElementQueryPage>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/accessibility/elements/list`,
        { desktop_id: this.id, desktop_generation: this.generation, ...spec },
        { ...options, maxRequestBytes: 512 * 1024, maxResponseBytes: 8 * 1_048_576 },
      );
      validateElementPage(response, this);
      return response;
    },
    query: async (
      spec: ElementQuerySpec,
      options: RequestOptions = {},
    ): Promise<ElementQueryPage> => {
      const request: ElementQueryRequest = {
        desktop_id: this.id,
        desktop_generation: this.generation,
        ...spec,
      };
      const response = await this.#transport.request<ElementQueryPage>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/accessibility/elements/query`,
        request,
        { ...options, maxRequestBytes: 512 * 1024, maxResponseBytes: 8 * 1_048_576 },
      );
      validateElementPage(response, this);
      return response;
    },
    resolve: async (
      spec: ElementResolveSpec,
      options: RequestOptions = {},
    ): Promise<ElementResolveResult> => {
      const response = await this.#transport.request<ElementResolveResult>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/accessibility/elements/resolve`,
        { desktop_id: this.id, desktop_generation: this.generation, ...spec },
        { ...options, maxRequestBytes: 512 * 1024, maxResponseBytes: 2 * 1_048_576 },
      );
      validateScope(response, this);
      asCanonicalUInt64(response.atspi_generation, { allowZero: false });
      asCanonicalUInt64(response.snapshot_revision);
      validateElementEntry(response.element, this);
      return response;
    },
    snapshot: async (
      element: JsonObject,
      expansion?: JsonObject,
      options: RequestOptions = {},
    ): Promise<ElementSnapshotResult> => {
      validateElementRef(element, this);
      const response = await this.#transport.request<ElementSnapshotResult>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/accessibility/elements/snapshot`,
        {
          desktop_id: this.id,
          desktop_generation: this.generation,
          element,
          ...(expansion === undefined ? {} : { expansion }),
        },
        { ...options, maxRequestBytes: 512 * 1024, maxResponseBytes: 2 * 1_048_576 },
      );
      asCanonicalUInt64(response.snapshot_revision);
      validateElementEntry(response.element, this);
      return response;
    },
    wait: async (
      spec: ElementWaitSpec,
      options: RequestOptions = {},
    ): Promise<ElementWaitResult> => {
      if (!Number.isInteger(spec.timeout_ms) || spec.timeout_ms < 1 || spec.timeout_ms > 120_000) {
        throw new XenoteerError("invalid_request", "element wait timeout must be 1..120000ms");
      }
      const request: ElementWaitRequest = {
        desktop_id: this.id,
        desktop_generation: this.generation,
        ...spec,
      };
      const response = await this.#transport.request<ElementWaitResult>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.id)}/accessibility/elements/wait`,
        request,
        { ...options, maxRequestBytes: 512 * 1024, maxResponseBytes: 8 * 1_048_576, timeoutMs: spec.timeout_ms + 5_000 },
      );
      validateScope(response, this);
      asCanonicalUInt64(response.atspi_generation, { allowZero: false });
      asCanonicalUInt64(response.evaluated_revision);
      return response;
    },
  };

  /** @internal Shared generation fence used by immutable public handles. */
  assertCurrentReference(): void {
    this.#transport.state.assertGeneration(this.id, this.generation, "reference");
  }
}

function asRecord(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new XenoteerError("invalid_response", `${label} is invalid`);
  }
  return value as Record<string, unknown>;
}

function validateWindowRef(value: unknown, desktop: Desktop): void {
  const ref = asRecord(value, "window reference");
  if (
    ref["desktop_id"] !== desktop.id
    || ref["desktop_generation"] !== desktop.generation
    || !Number.isSafeInteger(ref["xid"])
    || (ref["xid"] as number) < 1
    || typeof ref["identity_hash"] !== "string"
  ) {
    throw new XenoteerError("stale_reference", "window reference is invalid or stale");
  }
  asCanonicalUInt64(ref["observed_generation"], { allowZero: false });
}

function validateWindowEntry(value: unknown, desktop: Desktop): void {
  const entry = asRecord(value, "window result");
  if (typeof entry["reference_token"] !== "string") {
    throw new XenoteerError("invalid_response", "window result omitted its reference token");
  }
  const snapshot = asRecord(entry["snapshot"], "window snapshot");
  validateWindowRef(snapshot["ref"], desktop);
  asCanonicalUInt64(snapshot["model_revision"]);
}

function validateWindowPage(
  value: WindowListPage | WindowQueryPage,
  desktop: Desktop,
): void {
  validateScope(value, desktop);
  asCanonicalUInt64(value.snapshot_revision);
  if (!Array.isArray(value.windows)) {
    throw new XenoteerError("invalid_response", "window page is invalid");
  }
  for (const entry of value.windows) validateWindowEntry(entry, desktop);
}

function validateElementRef(value: unknown, desktop: Desktop): void {
  const ref = asRecord(value, "element reference");
  if (
    ref["desktop_id"] !== desktop.id
    || ref["desktop_generation"] !== desktop.generation
    || typeof ref["object_path"] !== "string"
    || typeof ref["object_identity_hash"] !== "string"
  ) {
    throw new XenoteerError("stale_reference", "element reference is invalid or stale");
  }
  asCanonicalUInt64(ref["atspi_generation"], { allowZero: false });
  asCanonicalUInt64(ref["cache_sequence"], { allowZero: false });
}

function validateElementEntry(value: unknown, desktop: Desktop): void {
  const entry = asRecord(value, "element result");
  const snapshot = asRecord(entry["snapshot"], "element snapshot");
  validateElementRef(snapshot["ref"], desktop);
  asCanonicalUInt64(snapshot["revision"]);
}

function validateElementPage(value: ElementQueryPage, desktop: Desktop): void {
  validateScope(value, desktop);
  asCanonicalUInt64(value.atspi_generation, { allowZero: false });
  asCanonicalUInt64(value.snapshot_revision);
  if (!Array.isArray(value.elements)) {
    throw new XenoteerError("invalid_response", "element page is invalid");
  }
  for (const entry of value.elements) validateElementEntry(entry, desktop);
}

export function namedKey(name: string): KeyboardKeyIdentifier {
  const normalized = name
    .replace(/([a-z0-9])([A-Z])/gu, "$1_$2")
    .replace(/[\s-]+/gu, "_")
    .toLowerCase();
  if (!/^[a-z][a-z0-9_]{0,63}$/u.test(normalized)) {
    throw new XenoteerError("invalid_request", "named key is invalid");
  }
  return { kind: "named", name: normalized };
}

export function scalarKey(value: string): KeyboardKeyIdentifier {
  if ([...value].length !== 1) {
    throw new XenoteerError("invalid_request", "scalar key must contain exactly one Unicode scalar");
  }
  return { kind: "scalar", value };
}

export function customCommand(type: string, fields: JsonObject = {}): WireCommand {
  if (!/^[a-z][a-z0-9_]{0,127}$/u.test(type)) {
    throw new XenoteerError("invalid_request", "command type is invalid");
  }
  return { ...fields, type };
}
