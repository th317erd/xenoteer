// SPDX-License-Identifier: Apache-2.0

import type {
  ApiProblem,
  CleanupStatus,
  EffectStage,
  JsonObject,
  JsonValue,
  RetryAdvice,
} from "./protocol.generated.js";

const RETRY_ADVICE = new Set<RetryAdvice>([
  "never",
  "same_command_id",
  "after_resync",
  "after_backoff",
]);
const EFFECT_STAGES = new Set<EffectStage>([
  "none",
  "accepted",
  "outcome_unknown",
  "side_effect_observed",
  "pointer_moved",
  "button_pressed",
  "button_released",
  "pointer_clicked",
  "pointer_dragged",
  "pointer_scrolled",
  "key_pressed",
  "key_released",
  "keyboard_action_completed",
  "input_reset",
  "process_started",
  "process_signalled",
  "process_exited",
  "postcondition_met",
  "window_request_sent",
  "window_state_changed",
  "clipboard_ownership_changed",
  "text_inserted",
  "semantic_action_dispatched",
  "semantic_state_changed",
  "element_physically_clicked",
]);
const CLEANUP_STATUSES = new Set<CleanupStatus>([
  "not_required",
  "completed",
  "failed",
  "partial",
  "unknown",
]);

export type SdkErrorCode =
  | "authentication"
  | "permission"
  | "validation"
  | "unsupported"
  | "not_found"
  | "ambiguous"
  | "conflict"
  | "lease_conflict"
  | "resource"
  | "backend"
  | "partial_effect"
  | "invalid_base_url"
  | "invalid_token"
  | "invalid_request"
  | "invalid_response"
  | "unsupported_protocol"
  | "reversed_minor_range"
  | "unsupported_major"
  | "no_shared_minor"
  | "unsupported_version"
  | "desktop_unavailable"
  | "transport"
  | "request_cancelled"
  | "request_timeout"
  | "request_too_large"
  | "response_too_large"
  | "unexpected_http_status"
  | "lease_released"
  | "command_wait_timeout"
  | "client_closed"
  | "session_closed"
  | "websocket_timeout"
  | "backpressure"
  | "resync_required"
  | "generation_changed"
  | "stale_reference"
  | "outcome_unknown_after_restart"
  | "unsupported_response_variant";

/** Stable, redaction-safe SDK error. Request bodies and bearer values are never retained. */
export class XenoteerError extends Error {
  readonly code: SdkErrorCode;
  readonly status?: number;
  readonly requestId?: string;
  readonly commandId?: string;
  readonly problemCode?: string;
  readonly retry?: RetryAdvice;
  readonly effectStage?: EffectStage;
  readonly cleanup?: CleanupStatus;
  readonly details?: Readonly<JsonObject>;
  override readonly cause?: unknown;

  constructor(
    code: SdkErrorCode,
    message: string,
    options: {
      readonly status?: number;
      readonly requestId?: string;
      readonly commandId?: string;
      readonly problemCode?: string;
      readonly retry?: RetryAdvice;
      readonly effectStage?: EffectStage;
      readonly cleanup?: CleanupStatus;
      readonly details?: Readonly<JsonObject>;
      readonly cause?: unknown;
    } = {},
  ) {
    super(message);
    this.name = "XenoteerError";
    this.code = code;
    if (options.status !== undefined) this.status = options.status;
    if (options.requestId !== undefined) this.requestId = options.requestId;
    if (options.commandId !== undefined) this.commandId = options.commandId;
    if (options.problemCode !== undefined) this.problemCode = options.problemCode;
    if (options.retry !== undefined) this.retry = options.retry;
    if (options.effectStage !== undefined) this.effectStage = options.effectStage;
    if (options.cleanup !== undefined) this.cleanup = options.cleanup;
    if (options.details !== undefined) {
      this.details = Object.freeze(structuredClone(options.details));
    }
    if (options.cause !== undefined) {
      this.cause = Object.freeze({
        type: options.cause instanceof Error ? options.cause.name : typeof options.cause,
        detail: "<redacted>",
      });
    }
  }

  override toString(): string {
    return `${this.name}[${this.code}]: ${this.message}`;
  }
}

/** Reduces a server problem to fields the public protocol documents as safe. */
export function errorFromProblem(status: number, problem: ApiProblem): XenoteerError {
  const problemCode = typeof problem.code === "string"
    ? problem.code
    : undefined;
  const sdkCode = classifyProblem(status, problemCode);
  const details = safeProblemDetails(problem["details"]);
  const retry = typeof problem.retry === "string"
    && RETRY_ADVICE.has(problem.retry as RetryAdvice)
    ? problem.retry as RetryAdvice
    : undefined;
  const effectStage = typeof problem.effect_stage === "string"
    && EFFECT_STAGES.has(problem.effect_stage as EffectStage)
    ? problem.effect_stage as EffectStage
    : undefined;
  const cleanup = typeof problem.cleanup === "string"
    && CLEANUP_STATUSES.has(problem.cleanup as CleanupStatus)
    ? problem.cleanup as CleanupStatus
    : undefined;
  return new XenoteerError(
    sdkCode,
    problemCode !== undefined
      ? `Xenoteer request failed (${problemCode})`
      : `Xenoteer request failed with HTTP ${status}`,
    {
      status,
      ...(typeof problem.request_id === "string"
        ? { requestId: problem.request_id }
        : {}),
      ...(typeof problem.command_id === "string"
        ? { commandId: problem.command_id }
        : {}),
      ...(problemCode !== undefined
        ? { problemCode }
        : {}),
      ...(retry === undefined ? {} : { retry }),
      ...(effectStage === undefined ? {} : { effectStage }),
      ...(cleanup === undefined ? {} : { cleanup }),
      ...(details === undefined ? {} : { details }),
    },
  );
}

function classifyProblem(status: number, code: string | undefined): SdkErrorCode {
  if (status === 401 || code === "authentication_failed") return "authentication";
  if (status === 403 || code === "permission_denied") return "permission";
  if (code === "stale_reference" || code === "generation_mismatch") return "stale_reference";
  if (code === "ambiguous_target") return "ambiguous";
  if (status === 404) return "not_found";
  if (code?.startsWith("lease_") === true) return "lease_conflict";
  if (status === 409) return "conflict";
  if (status === 400 || status === 422) return "validation";
  if (status === 413 || status === 429 || status === 507) return "resource";
  if (status >= 500) return "backend";
  return "unexpected_http_status";
}

function safeProblemDetails(
  value: unknown,
): Readonly<JsonObject> | undefined {
  if (
    typeof value !== "object"
    || value === null
    || Array.isArray(value)
  ) {
    return undefined;
  }
  const entries = Object.entries(value);
  if (
    entries.length > 16
    || entries.some(([key]) => !/^[a-z0-9._-]{1,64}$/u.test(key))
  ) {
    return undefined;
  }
  let encoded: string;
  try {
    encoded = JSON.stringify(value);
  } catch {
    return undefined;
  }
  if (new TextEncoder().encode(encoded).byteLength > 4 * 1024) {
    return undefined;
  }
  return deepFreeze(
    Object.fromEntries(
      entries.map(([key, child]) => [key, redactProblemDetail(child, 0)]),
    ),
  ) as Readonly<JsonObject>;
}

function redactProblemDetail(value: unknown, depth: number): JsonValue {
  if (depth >= 16) return "<redacted>";
  if (typeof value === "string") return "<redacted>";
  if (
    value === null
    || typeof value === "boolean"
    || (typeof value === "number" && Number.isFinite(value))
  ) {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map((child) => redactProblemDetail(child, depth + 1));
  }
  if (typeof value === "object" && value !== null) {
    return Object.fromEntries(
      Object.entries(value).map(
        ([key, child]) => [key, redactProblemDetail(child, depth + 1)],
      ),
    );
  }
  return "<redacted>";
}

function deepFreeze<T>(value: T): T {
  if (typeof value === "object" && value !== null && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const child of Object.values(value)) deepFreeze(child);
  }
  return value;
}
