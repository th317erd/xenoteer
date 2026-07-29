// SPDX-License-Identifier: Apache-2.0

import { XenoteerError } from "./errors.js";
import type { CommandEnvelope, CommandResult } from "./protocol.generated.js";
import type { HttpTransport, RequestOptions } from "./transport.js";

const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;
const TERMINAL = new Set<CommandResult["lifecycle"]>([
  "succeeded",
  "failed",
  "cancelled_before_effect",
  "cancelled_after_effect",
  "deadline_before_effect",
  "deadline_after_effect",
]);
const LIFECYCLES = new Set([
  "accepted",
  "running",
  ...TERMINAL,
]);

export function validateCommandResult(value: unknown, commandId: string): CommandResult {
  if (
    typeof value !== "object"
    || value === null
    || Array.isArray(value)
    || !("command_id" in value)
    || value.command_id !== commandId
    || !UUID.test(value.command_id)
    || !("lifecycle" in value)
    || typeof value.lifecycle !== "string"
    || !LIFECYCLES.has(value.lifecycle)
    || !("effect_stage" in value)
    || typeof value.effect_stage !== "string"
    || !("accepted_at" in value)
    || typeof value.accepted_at !== "string"
    || !("warnings" in value)
    || !Array.isArray(value.warnings)
    || !Number.isFinite(Date.parse(value.accepted_at))
  ) {
    throw new XenoteerError("invalid_response", "server returned an invalid command result");
  }
  return value as unknown as CommandResult;
}

/**
 * A pre-I/O, exact command attempt. It retains one command ID, request ID, and
 * byte-equivalent envelope for explicit recovery after ambiguous transport loss.
 * Its diagnostic forms never include command bodies, lease IDs, or text.
 */
export class CommandSubmission {
  readonly id: string;
  readonly desktopGeneration: string;
  readonly #desktopId: string;
  readonly #transport: HttpTransport;
  readonly #encoded: string;
  readonly #commandType: string;

  constructor(transport: HttpTransport, envelope: CommandEnvelope) {
    this.#transport = transport;
    this.id = envelope.command_id;
    this.desktopGeneration = envelope.desktop_generation;
    this.#desktopId = envelope.desktop_id;
    this.#commandType = envelope.command.type;
    this.#encoded = JSON.stringify(structuredClone(envelope));
    Object.freeze(this);
  }

  get byteLength(): number {
    return new TextEncoder().encode(this.#encoded).byteLength;
  }

  /** Performs one attempt. Calling it again intentionally reuses the exact envelope bytes. */
  async send(options: RequestOptions = {}): Promise<CommandHandle> {
    this.#transport.state.assertGeneration(
      this.#desktopId,
      this.desktopGeneration,
      "command",
    );
    const result = await this.#transport.requestSerialized<CommandResult>(
      "POST",
      `/v1/desktops/${encodeURIComponent(this.#desktopId)}/commands`,
      this.#encoded,
      {
        ...options,
        headers: { ...options.headers, "idempotency-key": this.id },
        maxRequestBytes: 1_048_576,
        maxResponseBytes: 1_048_576,
      },
    );
    return new CommandHandle(
      this.#transport,
      this.#desktopId,
      this.desktopGeneration,
      validateCommandResult(result, this.id),
    );
  }

  toString(): string {
    return `CommandSubmission(id=${this.id}, type=${this.#commandType}, bytes=${this.byteLength}, body=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({
      id: this.id,
      type: this.#commandType,
      byteLength: this.byteLength,
      body: "<redacted>",
    });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}

/** Generation-bound handle. Local cancellation and dropping never replay or cancel server work. */
export class CommandHandle {
  readonly id: string;
  readonly desktopGeneration: string;
  readonly #desktopId: string;
  readonly #transport: HttpTransport;
  #latest: CommandResult;
  #terminalCleanup: (() => Promise<void>) | null = null;
  #cleanupAttempted = false;
  #cleanupError?: XenoteerError;

  constructor(
    transport: HttpTransport,
    desktopId: string,
    desktopGeneration: string,
    initial: CommandResult,
  ) {
    this.id = initial.command_id;
    this.desktopGeneration = desktopGeneration;
    this.#desktopId = desktopId;
    this.#transport = transport;
    this.#latest = validateCommandResult(initial, initial.command_id);
  }

  get latest(): CommandResult {
    this.#assertGeneration();
    return this.#latest;
  }

  get terminal(): boolean {
    this.#assertGeneration();
    return TERMINAL.has(this.#latest.lifecycle);
  }

  get cleanupError(): XenoteerError | undefined {
    return this.#cleanupError;
  }

  /**
   * Registers best-effort cleanup that runs only after terminal server evidence.
   * Cleanup failure is recorded but never changes the command outcome; artifact
   * expiry remains the reliable fallback.
   */
  async attachTerminalCleanup(cleanup: () => Promise<void>): Promise<this> {
    if (this.#terminalCleanup !== null || this.#cleanupAttempted) {
      throw new XenoteerError("invalid_request", "terminal cleanup is already registered");
    }
    this.#terminalCleanup = cleanup;
    await this.#runTerminalCleanup();
    return this;
  }

  async refresh(options: RequestOptions = {}): Promise<CommandResult> {
    this.#assertGeneration();
    const value = await this.#transport.request<CommandResult>(
      "GET",
      this.#path(),
      undefined,
      { ...options, maxResponseBytes: 1_048_576 },
    );
    this.#latest = validateCommandResult(value, this.id);
    await this.#runTerminalCleanup();
    return this.#latest;
  }

  async waitOnce(
    timeoutMs: number,
    options: RequestOptions = {},
  ): Promise<CommandResult> {
    this.#assertGeneration();
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 30_000) {
      throw new XenoteerError("invalid_request", "command wait timeout must be 1..30000ms");
    }
    const value = await this.#transport.request<CommandResult>(
      "GET",
      `${this.#path()}/wait?timeout_ms=${timeoutMs}`,
      undefined,
      { ...options, maxResponseBytes: 1_048_576, timeoutMs: timeoutMs + 5_000 },
    );
    this.#latest = validateCommandResult(value, this.id);
    await this.#runTerminalCleanup();
    return this.#latest;
  }

  /** Explicitly requests cooperative server cancellation. This is never called implicitly. */
  async cancel(options: RequestOptions = {}): Promise<CommandResult> {
    this.#assertGeneration();
    const value = await this.#transport.request<CommandResult>(
      "DELETE",
      this.#path(),
      undefined,
      { ...options, maxResponseBytes: 1_048_576 },
    );
    this.#latest = validateCommandResult(value, this.id);
    await this.#runTerminalCleanup();
    return this.#latest;
  }

  /**
   * Repeats only the server-side wait/read operation until terminal. It never
   * resubmits the mutation and a local AbortSignal never cancels server work.
   */
  async waitUntilTerminal(
    overallTimeoutMs: number,
    options: RequestOptions = {},
  ): Promise<CommandResult> {
    this.#assertGeneration();
    if (
      !Number.isInteger(overallTimeoutMs)
      || overallTimeoutMs < 1
      || overallTimeoutMs > 3_600_000
    ) {
      throw new XenoteerError("invalid_request", "overall command wait must be 1..3600000ms");
    }
    const deadline = performance.now() + overallTimeoutMs;
    while (!this.terminal) {
      const remaining = Math.ceil(deadline - performance.now());
      if (remaining <= 0) {
        throw new XenoteerError(
          "command_wait_timeout",
          "local command wait timed out; server command was not cancelled",
        );
      }
      await this.waitOnce(Math.min(remaining, 30_000), options);
    }
    return this.#latest;
  }

  #path(): string {
    return `/v1/desktops/${encodeURIComponent(this.#desktopId)}/commands/${encodeURIComponent(this.id)}`;
  }

  #assertGeneration(): void {
    this.#transport.state.assertGeneration(
      this.#desktopId,
      this.desktopGeneration,
      "command",
    );
  }

  async #runTerminalCleanup(): Promise<void> {
    if (!this.terminal || this.#terminalCleanup === null || this.#cleanupAttempted) return;
    this.#cleanupAttempted = true;
    const cleanup = this.#terminalCleanup;
    this.#terminalCleanup = null;
    try {
      await cleanup();
    } catch (cause) {
      this.#cleanupError = new XenoteerError(
        "transport",
        "terminal artifact cleanup failed; server expiry remains authoritative",
        { cause },
      );
    }
  }

  toString(): string {
    return `CommandHandle(id=${this.id}, lifecycle=${this.#latest.lifecycle})`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({ id: this.id, lifecycle: this.#latest.lifecycle });
  }

  [inspectSymbol](): string {
    return this.toString();
  }
}
