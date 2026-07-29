// SPDX-License-Identifier: Apache-2.0

import { CommandHandle } from "./command.js";
import type { Desktop, SubmitOptions } from "./desktop.js";
import { namedKey } from "./desktop.js";
import { Clipboard } from "./domains.js";
import { XenoteerError } from "./errors.js";
import type {
  KeyboardKeyIdentifier,
  LeaseState,
  WireCommand,
} from "./protocol.generated.js";
import type { HttpTransport, RequestOptions } from "./transport.js";

function uuid(): string {
  return globalThis.crypto.randomUUID();
}

const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;

function validateLeaseState(state: LeaseState, desktop: Desktop): LeaseState {
  if (
    state.desktop_id !== desktop.id
    || state.desktop_generation !== desktop.generation
    || state.state !== "held_by_caller"
    || typeof state.lease_id !== "string"
    || !UUID.test(state.lease_id)
    || typeof state.expires_at !== "string"
    || !Number.isFinite(Date.parse(state.expires_at))
  ) {
    throw new XenoteerError("invalid_response", "server returned an invalid owned lease state");
  }
  return Object.freeze(structuredClone(state));
}

/** Physical mouse API available only through an explicit owned control lease. */
export class Mouse {
  readonly #lease: ControlLease;

  constructor(lease: ControlLease) {
    this.#lease = lease;
  }

  async move(
    x: number,
    y: number,
    options: SubmitOptions & { readonly durationMs?: number } = {},
  ): Promise<CommandHandle> {
    if (
      !Number.isInteger(x)
      || !Number.isInteger(y)
      || x < -2_147_483_648
      || x > 2_147_483_647
      || y < -2_147_483_648
      || y > 2_147_483_647
    ) {
      throw new XenoteerError("invalid_request", "pointer coordinates must be signed 32-bit integers");
    }
    if (
      options.durationMs !== undefined
      && (!Number.isInteger(options.durationMs) || options.durationMs < 0 || options.durationMs > 10_000)
    ) {
      throw new XenoteerError("invalid_request", "pointer duration must be 0..10000ms");
    }
    const { durationMs, ...submitOptions } = options;
    return await this.#lease.submit(
      {
        type: "pointer_move",
        target: { x, y },
        duration_ms: durationMs ?? null,
        curve: "smooth",
      },
      submitOptions,
    );
  }

  async moveRelative(
    dx: number,
    dy: number,
    options: SubmitOptions & { readonly durationMs?: number } = {},
  ): Promise<CommandHandle> {
    validatePoint(dx, dy);
    const { durationMs, ...submit } = options;
    validateDuration(durationMs);
    return await this.#lease.submit(
      {
        type: "pointer_move_relative",
        delta: { x: dx, y: dy },
        duration_ms: durationMs ?? null,
        curve: "smooth",
      },
      submit,
    );
  }

  async click(
    x?: number,
    y?: number,
    options: SubmitOptions & {
      readonly button?: "left" | "middle" | "right" | "back" | "forward";
      readonly count?: number;
      readonly durationMs?: number;
      readonly preClickDwellMs?: number;
      readonly pressDurationMs?: number;
      readonly interClickIntervalMs?: number;
    } = {},
  ): Promise<CommandHandle> {
    if ((x === undefined) !== (y === undefined)) {
      throw new XenoteerError("invalid_request", "pointer click requires both coordinates or neither");
    }
    if (x !== undefined && y !== undefined) validatePoint(x, y);
    const {
      button = "left",
      count = 1,
      durationMs,
      preClickDwellMs = 0,
      pressDurationMs = 0,
      interClickIntervalMs = 0,
      ...submit
    } = options;
    validateDuration(durationMs);
    validateInteger(count, 1, 5, "click count");
    validateInteger(preClickDwellMs, 0, 10_000, "pre-click dwell");
    validateInteger(pressDurationMs, 0, 10_000, "press duration");
    validateInteger(interClickIntervalMs, 0, 249, "inter-click interval");
    return await this.#lease.submit(
      {
        type: "pointer_click",
        target: x === undefined || y === undefined
          ? { kind: "current" }
          : { kind: "root", point: { x, y } },
        button,
        count,
        curve: "smooth",
        duration_ms: durationMs ?? null,
        pre_click_dwell_ms: preClickDwellMs,
        press_duration_ms: pressDurationMs,
        inter_click_interval_ms: interClickIntervalMs,
      },
      submit,
    );
  }

  async drag(
    x: number,
    y: number,
    options: SubmitOptions & {
      readonly button?: "left" | "middle" | "right" | "back" | "forward";
      readonly durationMs?: number;
      readonly relative?: boolean;
      readonly pressDwellMs?: number;
      readonly releaseDwellMs?: number;
    } = {},
  ): Promise<CommandHandle> {
    validatePoint(x, y);
    const {
      button = "left",
      durationMs,
      relative = false,
      pressDwellMs = 0,
      releaseDwellMs = 0,
      ...submit
    } = options;
    validateDuration(durationMs);
    validateInteger(pressDwellMs, 0, 10_000, "press dwell");
    validateInteger(releaseDwellMs, 0, 10_000, "release dwell");
    return await this.#lease.submit(
      {
        type: "pointer_drag",
        target: relative
          ? { kind: "relative", delta: { x, y } }
          : { kind: "root", point: { x, y } },
        button,
        curve: "smooth",
        duration_ms: durationMs ?? null,
        press_dwell_ms: pressDwellMs,
        release_dwell_ms: releaseDwellMs,
      },
      submit,
    );
  }

  async scroll(
    direction: "up" | "down" | "left" | "right",
    count: number,
    options: SubmitOptions & { readonly intervalMs?: number } = {},
  ): Promise<CommandHandle> {
    const { intervalMs = 16, ...submit } = options;
    validateInteger(count, 1, 1000, "scroll count");
    validateInteger(intervalMs, 0, 1000, "scroll interval");
    return await this.#lease.submit(
      { type: "pointer_scroll", direction, count, interval_ms: intervalMs },
      submit,
    );
  }
}

function validatePoint(x: number, y: number): void {
  if (
    !Number.isInteger(x)
    || !Number.isInteger(y)
    || x < -2_147_483_648
    || x > 2_147_483_647
    || y < -2_147_483_648
    || y > 2_147_483_647
  ) {
    throw new XenoteerError("invalid_request", "pointer coordinates must be signed 32-bit integers");
  }
}

function validateDuration(value: number | undefined): void {
  if (value !== undefined) validateInteger(value, 0, 10_000, "pointer duration");
}

function validateInteger(value: number, minimum: number, maximum: number, label: string): void {
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw new XenoteerError("invalid_request", `${label} must be ${minimum}..${maximum}`);
  }
}

/** Physical keyboard API available only through an explicit owned control lease. */
export class Keyboard {
  readonly #lease: ControlLease;

  constructor(lease: ControlLease) {
    this.#lease = lease;
  }

  async press(
    key: string | KeyboardKeyIdentifier,
    options: SubmitOptions & { readonly holdMs?: number } = {},
  ): Promise<CommandHandle> {
    const { holdMs = 0, ...submitOptions } = options;
    validateHold(holdMs);
    return await this.#lease.submit(
      {
        type: "keyboard_press",
        key: typeof key === "string" ? namedKey(key) : key,
        hold_ms: holdMs,
      },
      submitOptions,
    );
  }

  async chord(
    keys: readonly (string | KeyboardKeyIdentifier)[],
    options: SubmitOptions & { readonly holdMs?: number } = {},
  ): Promise<CommandHandle> {
    if (keys.length < 1 || keys.length > 16) {
      throw new XenoteerError("invalid_request", "keyboard chord must contain 1..16 keys");
    }
    const { holdMs = 0, ...submitOptions } = options;
    validateHold(holdMs);
    return await this.#lease.submit(
      {
        type: "keyboard_chord",
        keys: keys.map((key) => typeof key === "string" ? namedKey(key) : key),
        hold_ms: holdMs,
      },
      submitOptions,
    );
  }

  async down(
    keycode: number,
    options: SubmitOptions & { readonly allowRedundant?: boolean } = {},
  ): Promise<CommandHandle> {
    validateInteger(keycode, 8, 255, "keycode");
    const { allowRedundant = false, ...submit } = options;
    return await this.#lease.submit(
      { type: "keyboard_key_down", keycode, allow_redundant: allowRedundant },
      submit,
    );
  }

  async up(
    keycode: number,
    options: SubmitOptions & { readonly allowRedundant?: boolean } = {},
  ): Promise<CommandHandle> {
    validateInteger(keycode, 8, 255, "keycode");
    const { allowRedundant = false, ...submit } = options;
    return await this.#lease.submit(
      { type: "keyboard_key_up", keycode, allow_redundant: allowRedundant },
      submit,
    );
  }

  async sequence(
    steps: readonly {
      readonly keys: readonly (string | KeyboardKeyIdentifier)[];
      readonly delayBeforeMs?: number;
      readonly holdMs?: number;
    }[],
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    if (steps.length < 1 || steps.length > 1024) {
      throw new XenoteerError("invalid_request", "keyboard sequence must contain 1..1024 steps");
    }
    const commandSteps = steps.map((step) => {
      if (step.keys.length < 1 || step.keys.length > 16) {
        throw new XenoteerError("invalid_request", "keyboard sequence step must contain 1..16 keys");
      }
      const delay = step.delayBeforeMs ?? 0;
      const hold = step.holdMs ?? 0;
      validateInteger(delay, 0, 10_000, "keyboard step delay");
      validateInteger(hold, 0, 10_000, "keyboard step hold");
      return {
        keys: step.keys.map((key) => typeof key === "string" ? namedKey(key) : key),
        delay_before_ms: delay,
        hold_ms: hold,
      };
    });
    return await this.#lease.submit(
      { type: "keyboard_sequence", steps: commandSteps },
      options,
    );
  }

  async insertText(
    text: string,
    target: import("./protocol.generated.js").JsonObject,
    options: SubmitOptions & {
      readonly strategy?: "auto" | "semantic" | "physical" | "clipboard" | "physical_extended";
      readonly autoPolicy?: import("./protocol.generated.js").JsonObject | null;
      readonly semanticOptions?: import("./protocol.generated.js").JsonObject | null;
      readonly clipboardOptions?: import("./protocol.generated.js").JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    const bytes = new TextEncoder().encode(text);
    if (bytes.byteLength > 256 * 1024) {
      throw new XenoteerError(
        "request_too_large",
        "inline inserted text exceeds 256 KiB; use an explicit artifact-backed command",
      );
    }
    const {
      strategy = "auto",
      autoPolicy = null,
      semanticOptions = null,
      clipboardOptions = null,
      ...submit
    } = options;
    return await this.#lease.submit(
      {
        type: "text_insert",
        text: { source: "inline", text },
        target,
        strategy,
        auto_policy: autoPolicy,
        semantic_options: semanticOptions,
        clipboard_options: clipboardOptions,
      },
      submit,
    );
  }
}

function validateHold(holdMs: number): void {
  if (!Number.isInteger(holdMs) || holdMs < 0 || holdMs > 10_000) {
    throw new XenoteerError("invalid_request", "keyboard hold must be 0..10000ms");
  }
}

/** Explicit generation-bound lease. Drop/GC cannot claim asynchronous release. */
export class ControlLease implements AsyncDisposable {
  readonly #desktop: Desktop;
  readonly #transport: HttpTransport;
  readonly #ttlMs?: number;
  #state: LeaseState;
  #active = true;
  #renewalTimer: ReturnType<typeof setTimeout> | undefined;
  #renewalFailure?: XenoteerError;
  readonly #unregisterReconnect: () => void;
  readonly #unregisterClose: () => void;

  constructor(
    desktop: Desktop,
    transport: HttpTransport,
    state: LeaseState,
    ttlMs?: number,
  ) {
    state = validateLeaseState(state, desktop);
    this.#desktop = desktop;
    this.#transport = transport;
    this.#state = state;
    if (ttlMs !== undefined) this.#ttlMs = ttlMs;
    this.#unregisterReconnect = transport.state.registerReconnect(async () => {
      await this.#recoverAfterReconnect();
    });
    this.#unregisterClose = transport.state.register(() => {
      this.stopAutoRenewal();
    });
  }

  get id(): string {
    this.#transport.state.assertOpen();
    const id = this.#state.lease_id;
    if (typeof id !== "string") {
      throw new XenoteerError("lease_released", "controller lease was released");
    }
    return id;
  }

  get expiresAt(): string {
    this.#transport.state.assertOpen();
    const expires = this.#state.expires_at;
    if (typeof expires !== "string") {
      throw new XenoteerError("lease_released", "controller lease was released");
    }
    return expires;
  }

  get active(): boolean {
    return this.#active && this.#renewalFailure === undefined;
  }

  get mouse(): Mouse {
    return new Mouse(this);
  }

  get keyboard(): Keyboard {
    return new Keyboard(this);
  }

  get clipboard(): {
    readonly setText: (
      text: string,
      options?: SubmitOptions & { readonly selection?: import("./protocol.generated.js").SelectionName },
    ) => Promise<CommandHandle>;
    readonly clear: (
      selection?: import("./protocol.generated.js").SelectionName,
      options?: SubmitOptions,
    ) => Promise<CommandHandle>;
  } {
    const clipboard = new Clipboard(this.#desktop, this.#transport);
    return {
      setText: async (text, options = {}) => await clipboard.setText(text, this.id, options),
      clear: async (selection = "clipboard", options = {}) => {
        return await clipboard.clear(this.id, selection, options);
      },
    };
  }

  async submit(
    command: WireCommand,
    options: Omit<SubmitOptions, "leaseId"> = {},
  ): Promise<CommandHandle> {
    this.#ensureActive();
    return await this.#desktop.submit(command, { ...options, leaseId: this.id });
  }

  async renew(options: RequestOptions = {}): Promise<LeaseState> {
    this.#ensureActive();
    const body = {
      protocol_version: this.#desktop.protocol,
      request_id: uuid(),
      desktop_id: this.#desktop.id,
      desktop_generation: this.#desktop.generation,
      lease_id: this.id,
      ...(this.#ttlMs === undefined ? {} : { ttl_ms: this.#ttlMs }),
    };
    try {
      const state = await this.#transport.request<LeaseState>(
        "POST",
        `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/lease/${encodeURIComponent(this.id)}/renew`,
        body,
        options,
      );
      const validated = validateLeaseState(state, this.#desktop);
      if (state.lease_id !== this.id) {
        throw new XenoteerError("invalid_response", "lease renewal changed capability identity");
      }
      this.#state = validated;
      return validated;
    } catch (cause) {
      this.#failLease("controller lease renewal failed", cause);
      throw this.#renewalFailure;
    }
  }

  async release(options: RequestOptions = {}): Promise<LeaseState> {
    this.#ensureActive();
    const leaseId = this.id;
    const body = {
      protocol_version: this.#desktop.protocol,
      request_id: uuid(),
      desktop_id: this.#desktop.id,
      desktop_generation: this.#desktop.generation,
      lease_id: leaseId,
    };
    const state = await this.#transport.request<LeaseState>(
      "DELETE",
      `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/lease/${encodeURIComponent(leaseId)}`,
      body,
      options,
    );
    if (
      state.desktop_id !== this.#desktop.id
      || state.desktop_generation !== this.#desktop.generation
      || state.state !== "vacant"
      || state.lease_id != null
    ) {
      throw new XenoteerError("invalid_response", "server returned an invalid released lease state");
    }
    this.#state = Object.freeze(structuredClone(state));
    this.#active = false;
    this.stopAutoRenewal();
    this.#unregisterReconnect();
    this.#unregisterClose();
    return this.#state;
  }

  async [Symbol.asyncDispose](): Promise<void> {
    if (this.#active) await this.release();
  }

  #ensureActive(): void {
    this.#transport.state.assertGeneration(
      this.#desktop.id,
      this.#desktop.generation,
      "lease",
    );
    if (this.#renewalFailure !== undefined) throw this.#renewalFailure;
    if (!this.#active) {
      throw new XenoteerError("lease_released", "controller lease was released");
    }
  }

  /** @internal Starts renewal only for an explicitly scoped lease. */
  startAutoRenewal(): void {
    this.#ensureActive();
    if (this.#renewalTimer !== undefined) {
      throw new XenoteerError("invalid_request", "lease auto-renewal is already active");
    }
    const requested = this.#ttlMs
      ?? Math.max(1, Date.parse(this.expiresAt) - Date.now());
    const interval = Math.max(1, Math.min(30_000, Math.floor(requested / 2)));
    const tick = (): void => {
      this.#renewalTimer = setTimeout(() => {
        void this.renew().then(() => {
          if (this.active) tick();
        }).catch(() => {
          // renew() records a sticky, redaction-safe failure for subsequent calls.
        });
      }, interval);
    };
    tick();
  }

  stopAutoRenewal(): void {
    if (this.#renewalTimer !== undefined) {
      clearTimeout(this.#renewalTimer);
      this.#renewalTimer = undefined;
    }
  }

  async #recoverAfterReconnect(): Promise<void> {
    if (!this.#active || this.#renewalFailure !== undefined) return;
    try {
      this.#transport.state.assertGeneration(
        this.#desktop.id,
        this.#desktop.generation,
        "lease",
      );
      const state = await this.#transport.request<LeaseState>(
        "GET",
        `/v1/desktops/${encodeURIComponent(this.#desktop.id)}/lease`,
        undefined,
        { maxResponseBytes: 64 * 1024 },
      );
      const validated = validateLeaseState(state, this.#desktop);
      if (validated.lease_id !== this.id) {
        throw new XenoteerError(
          "lease_released",
          "controller lease identity changed during reconnect",
        );
      }
      this.#state = validated;
    } catch (cause) {
      this.#failLease(
        "controller lease could not be recovered after reconnect",
        cause,
      );
    }
  }

  #failLease(message: string, cause: unknown): void {
    this.stopAutoRenewal();
    this.#active = false;
    this.#renewalFailure = new XenoteerError("lease_released", message, { cause });
  }

  toString(): string {
    return `ControlLease(active=${this.#active}, leaseId=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({ active: this.#active, leaseId: "<redacted>" });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}
