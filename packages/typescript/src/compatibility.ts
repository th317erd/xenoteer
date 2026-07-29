// SPDX-License-Identifier: Apache-2.0

import { validateCommandResult } from "./command.js";
import { XenoteerError } from "./errors.js";
import type {
  CommandEnvelope,
  CommandResult,
  JsonObject,
  ProtocolVersion,
  StatusResponse,
} from "./protocol.generated.js";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;
const DESKTOP_STATES = new Set([
  "booting",
  "probing",
  "ready",
  "degraded",
  "draining",
  "stopped",
  "failed",
]);
const CAPABILITY_STATUSES = new Set([
  "available",
  "degraded",
  "unavailable",
  "disabled",
]);
const CAPABILITY_ID = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/u;
const REASON_CODE = /^[a-z0-9._-]+$/u;
const ENVELOPE_FIELDS = new Set([
  "protocol_version",
  "request_id",
  "command_id",
  "desktop_id",
  "desktop_generation",
  "lease_id",
  "deadline",
  "trace_policy",
  "command",
]);
const PROCESS_REF_FIELDS = new Set([
  "desktop_generation",
  "pid",
  "proc_start_ticks",
  "launch_id",
]);
const WINDOW_REF_FIELDS = new Set([
  "desktop_id",
  "desktop_generation",
  "xid",
  "observed_generation",
  "identity_hash",
]);
const ARTIFACT_REF_FIELDS = new Set([
  "artifact_id",
  "purpose",
  "desktop_id",
  "desktop_generation",
  "content_type",
  "content_length",
  "sha256",
  "created_at",
  "expires_at",
]);
const ELEMENT_REF_FIELDS = new Set([
  "desktop_id",
  "desktop_generation",
  "atspi_generation",
  "application",
  "object_path",
  "object_identity_hash",
  "cache_sequence",
]);
const APPLICATION_REF_FIELDS = new Set([
  "desktop_id",
  "desktop_generation",
  "atspi_generation",
  "unique_bus_name",
  "root_object_path",
  "app_instance_generation",
  "identity_hash",
]);
const COMMAND_FIELDS = new Map<string, ReadonlySet<string>>([
  ["desktop_probe", new Set(["type"])],
  ["pointer_move", new Set(["type", "target", "curve", "duration_ms"])],
  ["pointer_move_relative", new Set(["type", "delta", "curve", "duration_ms"])],
  ["pointer_click", new Set(["type", "target", "button", "count", "curve", "duration_ms", "pre_click_dwell_ms", "press_duration_ms", "inter_click_interval_ms"])],
  ["pointer_drag", new Set(["type", "target", "button", "curve", "duration_ms", "press_dwell_ms", "release_dwell_ms"])],
  ["pointer_scroll", new Set(["type", "direction", "count", "interval_ms"])],
  ["pointer_button_down", new Set(["type", "button", "allow_redundant"])],
  ["pointer_button_up", new Set(["type", "button", "allow_redundant"])],
  ["keyboard_key_down", new Set(["type", "keycode", "allow_redundant"])],
  ["keyboard_key_up", new Set(["type", "keycode", "allow_redundant"])],
  ["keyboard_press", new Set(["type", "key", "hold_ms"])],
  ["keyboard_chord", new Set(["type", "keys", "hold_ms"])],
  ["keyboard_sequence", new Set(["type", "steps"])],
  ["input_reset", new Set(["type"])],
  ["application_launch", new Set(["type", "application", "arguments"])],
  ["process_terminate", new Set(["type", "process", "grace_ms"])],
  ["process_status", new Set(["type", "process"])],
  ["window_activate", new Set(["type", "window", "switch_workspace", "fallback"])],
  ["window_close", new Set(["type", "window", "wait_for"])],
  ["window_set_state", new Set(["type", "window", "state", "desired"])],
  ["window_minimize", new Set(["type", "window", "desired"])],
  ["window_move_resize", new Set(["type", "window", "relative_to", "geometry", "bounds_policy"])],
  ["window_move_to_workspace", new Set(["type", "window", "workspace"])],
  ["window_stack", new Set(["type", "window", "mode", "sibling"])],
  ["selection_set", new Set(["type", "selection", "content"])],
  ["selection_clear", new Set(["type", "selection"])],
  ["text_insert", new Set(["type", "text", "target", "strategy", "auto_policy", "clipboard_options", "semantic_options"])],
  ["element_invoke", new Set(["type", "element", "action", "allow_disabled", "postcondition"])],
  ["element_focus", new Set(["type", "element", "require_window_focus_correlation", "postcondition"])],
  ["element_set_value", new Set(["type", "element", "value", "tolerance", "postcondition"])],
  ["element_selection", new Set(["type", "element", "operation", "postcondition"])],
  ["element_set_text", new Set(["type", "element", "text", "selection", "verify_length_only", "postcondition"])],
  ["element_insert_text", new Set(["type", "element", "offset", "text", "selection", "verify_length_only", "postcondition"])],
  ["element_scroll", new Set(["type", "element", "target", "postcondition"])],
  ["element_physical_click", new Set(["type", "element", "minimum_correlation", "point_policy", "scroll_policy", "activation_policy", "occlusion_policy", "button", "count", "interval_ms", "curve", "settle_timeout_ms", "move_duration_ms", "window", "postcondition"])],
]);
const KNOWN_OUTCOMES = new Set([
  "probe",
  "application_launched",
  "process_status",
  "process_terminated",
  "acknowledged",
  "window_control",
  "text_inserted",
  "element_action",
  "element_physical_click",
]);

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

function utf8Length(text: string, stopAfter: number): number {
  let length = 0;
  for (const scalar of text) {
    const point = scalar.codePointAt(0) ?? 0;
    length += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
    if (length > stopAfter) return length;
  }
  return length;
}

function validBoundedText(value: unknown, maximum: number): value is string {
  return typeof value === "string"
    && utf8Length(value, maximum) >= 1
    && utf8Length(value, maximum) <= maximum
    && !/[\u0000-\u001f\u007f]/u.test(value);
}

function validReasonCode(value: unknown): value is string {
  return typeof value === "string"
    && REASON_CODE.test(value)
    && utf8Length(value, 128) >= 1
    && utf8Length(value, 128) <= 128;
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

function validateCapabilityReport(value: Record<string, unknown>): void {
  const capabilities = value["capabilities"];
  if (
    !Array.isArray(capabilities)
    || capabilities.length > 256
  ) {
    throw new XenoteerError(
      "invalid_response",
      "status capability report is invalid",
    );
  }
  for (const capability of capabilities) {
    if (
      !isObject(capability)
      || typeof capability["id"] !== "string"
      || !CAPABILITY_ID.test(capability["id"])
      || utf8Length(capability["id"], 128) < 1
      || utf8Length(capability["id"], 128) > 128
      || typeof capability["status"] !== "string"
      || !CAPABILITY_STATUSES.has(capability["status"])
    ) {
      throw new XenoteerError(
        "invalid_response",
        "status capability entry is invalid",
      );
    }
    const reason = capability["reason_code"];
    if (
      reason !== undefined
      && reason !== null
      && !validReasonCode(reason)
    ) {
      throw new XenoteerError(
        "invalid_response",
        "status capability reason code is invalid",
      );
    }
    const backendVersion = capability["backend_version"];
    if (
      backendVersion !== undefined
      && backendVersion !== null
      && !validBoundedText(backendVersion, 128)
    ) {
      throw new XenoteerError(
        "invalid_response",
        "status capability backend version is invalid",
      );
    }
  }
}

/** Validates frozen status fields while preserving additive response metadata. */
export function validateStatusResponse(value: unknown): StatusResponse {
  if (
    !isObject(value)
    || !validBoundedText(value["server_version"], 128)
    || !isVersion(value["protocol_min"])
    || !isVersion(value["protocol_max"])
    || typeof value["server_time"] !== "string"
    || !Number.isFinite(Date.parse(value["server_time"]))
    || !isObject(value["desktop"])
    || typeof value["desktop"]["id"] !== "string"
    || !UUID.test(value["desktop"]["id"])
    || value["desktop"]["id"] === "00000000-0000-0000-0000-000000000000"
    || typeof value["desktop"]["state"] !== "string"
    || !DESKTOP_STATES.has(value["desktop"]["state"])
    || !isObject(value["capabilities"])
    || !Array.isArray(value["capabilities"]["capabilities"])
  ) {
    throw new XenoteerError("invalid_response", "server returned an invalid status response");
  }
  const protocolMin = value["protocol_min"];
  const protocolMax = value["protocol_max"];
  if (
    protocolMin.major !== protocolMax.major
    || protocolMin.minor > protocolMax.minor
  ) {
    throw new XenoteerError("invalid_response", "server protocol range is invalid");
  }
  const generation = value["desktop"]["generation"];
  if (generation !== undefined && generation !== null && typeof generation !== "string") {
    throw new XenoteerError("invalid_response", "status desktop generation is invalid");
  }
  if (
    typeof generation === "string"
    && (!UUID.test(generation) || generation === "00000000-0000-0000-0000-000000000000")
  ) {
    throw new XenoteerError("invalid_response", "status desktop generation is invalid");
  }
  const reason = value["desktop"]["reason_code"];
  if (
    reason !== undefined
    && reason !== null
    && !validReasonCode(reason)
  ) {
    throw new XenoteerError("invalid_response", "status desktop reason code is invalid");
  }
  validateCapabilityReport(value["capabilities"]);
  return deepFreeze(structuredClone(value)) as unknown as StatusResponse;
}

function rejectUnknownFields(
  value: Record<string, unknown>,
  allowed: ReadonlySet<string>,
  label: string,
): void {
  if (Object.keys(value).some((field) => !allowed.has(field))) {
    throw new XenoteerError("invalid_request", `${label} contains an unknown field`);
  }
}

function validateAuthorityReferences(command: Record<string, unknown>): void {
  const process = command["process"];
  if (process !== undefined) {
    if (!isObject(process)) {
      throw new XenoteerError("invalid_request", "process reference is invalid");
    }
    rejectUnknownFields(process, PROCESS_REF_FIELDS, "process reference");
  }
  const window = command["window"];
  if (window !== undefined && window !== null) {
    if (!isObject(window)) {
      throw new XenoteerError("invalid_request", "window reference is invalid");
    }
    rejectUnknownFields(window, WINDOW_REF_FIELDS, "window reference");
  }
  const element = command["element"];
  if (element !== undefined) {
    if (!isObject(element)) {
      throw new XenoteerError("invalid_request", "element reference is invalid");
    }
    rejectUnknownFields(element, ELEMENT_REF_FIELDS, "element reference");
    const application = element["application"];
    if (!isObject(application)) {
      throw new XenoteerError("invalid_request", "element application reference is invalid");
    }
    rejectUnknownFields(application, APPLICATION_REF_FIELDS, "application reference");
  }
  const content = command["content"];
  if (isObject(content) && content["source"] === "artifact") {
    const artifact = content["artifact"];
    if (!isObject(artifact)) {
      throw new XenoteerError("invalid_request", "artifact reference is invalid");
    }
    rejectUnknownFields(artifact, ARTIFACT_REF_FIELDS, "artifact reference");
  }
  const text = command["text"];
  if (isObject(text) && text["source"] === "artifact") {
    const artifact = text["artifact"];
    if (!isObject(artifact)) {
      throw new XenoteerError("invalid_request", "artifact reference is invalid");
    }
    rejectUnknownFields(artifact, ARTIFACT_REF_FIELDS, "artifact reference");
  }
}

/**
 * Strict request-direction decoder. Server outputs are additive; client-authored
 * envelopes and nested capability-bearing references are deliberately closed.
 */
export function validateCommandEnvelopeInput(value: unknown): CommandEnvelope {
  if (!isObject(value)) {
    throw new XenoteerError("invalid_request", "command envelope is invalid");
  }
  rejectUnknownFields(value, ENVELOPE_FIELDS, "command envelope");
  if (
    !isObject(value["protocol_version"])
    || !Number.isInteger(value["protocol_version"]["major"])
    || !Number.isInteger(value["protocol_version"]["minor"])
    || typeof value["request_id"] !== "string"
    || !UUID.test(value["request_id"])
    || typeof value["command_id"] !== "string"
    || !UUID.test(value["command_id"])
    || typeof value["desktop_id"] !== "string"
    || !UUID.test(value["desktop_id"])
    || typeof value["desktop_generation"] !== "string"
    || !UUID.test(value["desktop_generation"])
    || !isObject(value["command"])
    || typeof value["command"]["type"] !== "string"
  ) {
    throw new XenoteerError("invalid_request", "command envelope is invalid");
  }
  const commandFields = COMMAND_FIELDS.get(value["command"]["type"]);
  if (commandFields === undefined) {
    throw new XenoteerError("invalid_request", "command type is not part of protocol v1");
  }
  rejectUnknownFields(value["command"], commandFields, "command");
  validateAuthorityReferences(value["command"]);
  return deepFreeze(structuredClone(value)) as unknown as CommandEnvelope;
}

export interface KnownStatusResponse {
  readonly kind: "status";
  readonly raw: Readonly<Record<string, unknown>>;
}

export interface KnownCommandResponse {
  readonly kind: "command";
  readonly result: CommandResult;
  readonly raw: Readonly<Record<string, unknown>>;
}

export interface UnknownAdditiveResponse {
  readonly kind: "unknown_message";
  readonly raw: Readonly<Record<string, unknown>>;
}

export type DecodedServerResponse =
  | KnownStatusResponse
  | KnownCommandResponse
  | UnknownAdditiveResponse;

/** Bounded transport callers feed this only after the byte ceiling has passed. */
export function decodeAdditiveServerResponse(value: unknown): DecodedServerResponse {
  if (!isObject(value)) {
    throw new XenoteerError("invalid_response", "server response is not an object");
  }
  const raw = deepFreeze(structuredClone(value));
  if (!Object.hasOwn(value, "type")) {
    validateStatusResponse(value);
    return { kind: "status", raw };
  }
  if (value["type"] === "command.result") {
    if (
      typeof value["request_id"] !== "string"
      || !UUID.test(value["request_id"])
      || !isObject(value["result"])
      || typeof value["result"]["command_id"] !== "string"
    ) {
      throw new XenoteerError("invalid_response", "command response is invalid");
    }
    const result = validateCommandResult(
      value["result"],
      value["result"]["command_id"],
    );
    if (
      result.outcome !== undefined
      && result.outcome !== null
      && isObject(result.outcome)
      && typeof result.outcome["type"] === "string"
      && !KNOWN_OUTCOMES.has(result.outcome["type"])
    ) {
      throw new XenoteerError(
        "unsupported_response_variant",
        "command outcome variant is not supported by this SDK",
        {
          details: {
            outcome_type: String(result.outcome["type"]).slice(0, 128),
            outcome: "<redacted>",
          },
        },
      );
    }
    return { kind: "command", result, raw };
  }
  return { kind: "unknown_message", raw };
}

export interface EffectClassification {
  readonly category: "success" | "cancelled" | "timeout" | "partial_effect" | "failed" | "pending";
  readonly afterEffect: boolean;
  readonly retryRequiresUserDecision: boolean;
  readonly outcome: CommandResult["outcome"];
  readonly error: CommandResult["error"];
  readonly warnings: CommandResult["warnings"];
}

/** Classifies only server evidence; it never infers that retry is effect-free. */
export function classifyCommandEffect(result: CommandResult): EffectClassification {
  const lifecycle = result.lifecycle;
  const afterEffect = lifecycle.endsWith("_after_effect")
    || (!["accepted", "running", "cancelled_before_effect", "deadline_before_effect"].includes(lifecycle)
      && result.effect_stage !== "accepted"
      && result.effect_stage !== "before_effect");
  let category: EffectClassification["category"];
  if (lifecycle === "succeeded") category = "success";
  else if (lifecycle.startsWith("cancelled_")) category = "cancelled";
  else if (lifecycle.startsWith("deadline_")) category = "timeout";
  else if (lifecycle === "failed" && afterEffect) category = "partial_effect";
  else if (lifecycle === "failed") category = "failed";
  else category = "pending";
  return Object.freeze({
    category,
    afterEffect,
    retryRequiresUserDecision: afterEffect,
    outcome: result.outcome,
    error: result.error,
    warnings: result.warnings,
  });
}

export function exactProtocolVersion(
  major: number,
  minor: number,
): ProtocolVersion {
  if (
    !Number.isInteger(major)
    || !Number.isInteger(minor)
    || major < 0
    || minor < 0
    || major > 65_535
    || minor > 65_535
  ) {
    throw new XenoteerError("invalid_request", "protocol version is invalid");
  }
  return Object.freeze({ major, minor });
}
