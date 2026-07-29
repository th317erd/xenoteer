// SPDX-License-Identifier: Apache-2.0
/*
 * Generated-shape facade for the frozen Xenoteer v1.0 JSON schemas.
 *
 * Request interfaces are intentionally closed by their builders. Server
 * response interfaces accept additive fields so compatible newer peers remain
 * observable. Precision-sensitive uint64 fields are always CanonicalUInt64,
 * never JavaScript number.
 */

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonObject | readonly JsonValue[];
export interface JsonObject {
  readonly [key: string]: JsonValue;
}

declare const canonicalUInt64Brand: unique symbol;
export type CanonicalUInt64 = string & {
  readonly [canonicalUInt64Brand]: "CanonicalUInt64";
};

export interface ProtocolVersion {
  readonly major: number;
  readonly minor: number;
  readonly [key: string]: unknown;
}

export interface ProtocolRange {
  readonly major: number;
  readonly minMinor: number;
  readonly maxMinor: number;
}

export type DesktopState =
  | "booting"
  | "probing"
  | "ready"
  | "degraded"
  | "draining"
  | "stopped"
  | "failed";

export interface DesktopStatus {
  readonly id: string;
  readonly generation?: string | null;
  readonly state: DesktopState;
  readonly reason_code?: string | null;
  readonly [key: string]: unknown;
}

export interface CapabilityReport {
  readonly capabilities: readonly JsonValue[];
  readonly [key: string]: unknown;
}

export interface StatusResponse {
  readonly server_version: string;
  readonly protocol_min: ProtocolVersion;
  readonly protocol_max: ProtocolVersion;
  readonly server_time: string;
  readonly desktop: DesktopStatus;
  readonly capabilities: CapabilityReport;
  readonly [key: string]: unknown;
}

export type CommandLifecycle =
  | "accepted"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled_before_effect"
  | "cancelled_after_effect"
  | "deadline_before_effect"
  | "deadline_after_effect";

export interface CommandResult {
  readonly command_id: string;
  readonly lifecycle: CommandLifecycle;
  readonly effect_stage: string;
  readonly accepted_at: string;
  readonly started_at?: string | null;
  readonly finished_at?: string | null;
  readonly outcome?: JsonValue | null;
  readonly error?: JsonValue | null;
  readonly warnings: readonly JsonValue[];
  readonly [key: string]: unknown;
}

export interface Point {
  readonly x: number;
  readonly y: number;
}

export interface PointerMoveCommand extends JsonObject {
  readonly type: "pointer_move";
  readonly target: Point & JsonObject;
  readonly duration_ms: number | null;
  readonly curve: "smooth" | "linear" | "instant";
}

export type KeyboardKeyIdentifier =
  | (JsonObject & { readonly kind: "named"; readonly name: string })
  | (JsonObject & { readonly kind: "scalar"; readonly value: string })
  | (JsonObject & { readonly kind: "raw"; readonly keycode: number });

export interface KeyboardPressCommand extends JsonObject {
  readonly type: "keyboard_press";
  readonly key: KeyboardKeyIdentifier;
  readonly hold_ms: number;
}

export interface KeyboardChordCommand extends JsonObject {
  readonly type: "keyboard_chord";
  readonly keys: readonly KeyboardKeyIdentifier[];
  readonly hold_ms: number;
}

export type WireCommand =
  | PointerMoveCommand
  | KeyboardPressCommand
  | KeyboardChordCommand
  | (JsonObject & { readonly type: string });

export interface CommandEnvelope {
  readonly protocol_version: ProtocolVersion;
  readonly request_id: string;
  readonly command_id: string;
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly lease_id: string | null;
  readonly deadline: string | null;
  readonly trace_policy: "none" | "normal" | "detailed" | null;
  readonly command: WireCommand;
}

export type SelectionName = "clipboard" | "primary";
export type ArtifactPurpose =
  | "clipboard_input"
  | "clipboard_output"
  | "screenshot"
  | "action_trace"
  | "support_bundle";

export interface ArtifactRef {
  readonly artifact_id: string;
  readonly purpose: ArtifactPurpose;
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly content_type: string;
  readonly content_length: number;
  readonly sha256: string;
  readonly created_at: string;
  readonly expires_at: string;
  readonly [key: string]: unknown;
}

export interface ClipboardReadRequest {
  readonly selection: SelectionName;
  readonly preferred_targets: readonly string[];
  readonly allow_binary_fallback: boolean;
}

export interface ClipboardReadResult {
  readonly selection: SelectionName;
  readonly revision: CanonicalUInt64;
  readonly evidence: JsonValue;
  readonly content: JsonValue;
  readonly [key: string]: unknown;
}

export interface ScreenshotRequest {
  readonly target: JsonObject;
  readonly format: "png" | "webp_lossless" | "bmp" | "raw_bgra";
  readonly include_cursor: boolean;
  readonly region?: JsonValue | null;
  readonly scale?: JsonValue | null;
  readonly max_bytes?: number | null;
}

export interface ScreenshotResult {
  readonly target: JsonObject;
  readonly source_region: JsonObject;
  readonly source_size: JsonObject;
  readonly limitation: string;
  readonly format: string;
  readonly size: JsonObject;
  readonly cursor: JsonObject;
  readonly sha256: string;
  readonly delivery: JsonObject;
  readonly raw?: JsonValue | null;
  readonly [key: string]: unknown;
}

export interface ViewerTicket {
  readonly ticket: string;
  readonly principal_id: string;
  readonly audience: "viewer_websocket";
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly origin: string;
  readonly mode: "view_only";
  readonly issued_at: string;
  readonly expires_at: string;
  readonly use_policy: "single_use";
  readonly [key: string]: unknown;
}

export interface ProcessRef extends JsonObject {
  readonly desktop_generation: string;
  readonly pid: number;
  readonly proc_start_ticks: CanonicalUInt64;
  readonly launch_id: string;
}

export type LeaseAvailability =
  | "vacant"
  | "held_by_caller"
  | "occupied"
  | "revoking"
  | "resetting";

export interface LeaseState {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly state: LeaseAvailability;
  readonly lease_id?: string | null;
  readonly expires_at?: string | null;
  readonly [key: string]: unknown;
}

export interface WindowQueryRequest {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly selector: JsonObject;
  readonly order: string;
  readonly limit?: number;
  readonly cursor?: string | null;
}

export interface WindowQueryPage {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly snapshot_revision: CanonicalUInt64;
  readonly windows: readonly JsonValue[];
  readonly next_cursor?: string | null;
  readonly [key: string]: unknown;
}

export interface WindowListPage extends WindowQueryPage {}
export interface WindowResolveResult {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly snapshot_revision: CanonicalUInt64;
  readonly window: JsonObject;
  readonly [key: string]: unknown;
}
export interface WindowSnapshotResult {
  readonly snapshot_revision: CanonicalUInt64;
  readonly window: JsonObject;
  readonly [key: string]: unknown;
}

export interface WindowWaitRequest {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly target: JsonObject;
  readonly predicate: JsonObject;
  readonly timeout_ms: number;
  readonly after_revision?: CanonicalUInt64 | null;
}

export interface WindowWaitResult {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly status: string;
  readonly evaluated_revision: CanonicalUInt64;
  readonly predicate_satisfied: boolean;
  readonly matched_count: number;
  readonly windows: readonly JsonValue[];
  readonly [key: string]: unknown;
}

export interface AccessibilityLimits {
  readonly max_visited_nodes: number;
  readonly max_depth: number;
  readonly max_matches: number;
  readonly timeout_ms: number;
}

export interface ElementQueryRequest {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly selector: JsonObject;
  readonly limit?: number;
  readonly cursor?: string | null;
  readonly limits?: AccessibilityLimits;
  readonly expansion?: JsonObject;
}

export interface ElementQueryPage {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly atspi_generation: CanonicalUInt64;
  readonly snapshot_revision: CanonicalUInt64;
  readonly order: string;
  readonly elements: readonly JsonValue[];
  readonly visited_nodes: number;
  readonly truncated: boolean;
  readonly warnings: readonly JsonValue[];
  readonly next_cursor?: string | null;
  readonly [key: string]: unknown;
}

export interface ElementResolveResult {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly atspi_generation: CanonicalUInt64;
  readonly snapshot_revision: CanonicalUInt64;
  readonly element: JsonObject;
  readonly [key: string]: unknown;
}
export interface ElementSnapshotResult {
  readonly snapshot_revision: CanonicalUInt64;
  readonly element: JsonObject;
  readonly [key: string]: unknown;
}

export interface ElementWaitRequest {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly target: JsonObject;
  readonly predicate: JsonObject;
  readonly timeout_ms: number;
  readonly allow_poll_fallback: boolean;
  readonly after_revision?: CanonicalUInt64 | null;
  readonly limits?: AccessibilityLimits;
  readonly expansion?: JsonObject;
}

export interface ElementWaitResult {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly atspi_generation: CanonicalUInt64;
  readonly status: string;
  readonly evaluated_revision: CanonicalUInt64;
  readonly predicate_satisfied: boolean;
  readonly matched_count: number;
  readonly elements: readonly JsonValue[];
  readonly poll_fallback_used: boolean;
  readonly truncated: boolean;
  readonly warnings: readonly JsonValue[];
  readonly [key: string]: unknown;
}

export interface EventWire {
  readonly desktop_id: string;
  readonly desktop_generation: string;
  readonly sequence: CanonicalUInt64;
  readonly topic: string;
  readonly payload: JsonValue;
  readonly [key: string]: unknown;
}

export interface EventMessageWire {
  readonly type: "event";
  readonly request_id: string;
  readonly event: EventWire;
  readonly [key: string]: unknown;
}

export type RetryAdvice =
  | "never"
  | "same_command_id"
  | "after_resync"
  | "after_backoff";

export type EffectStage =
  | "none"
  | "accepted"
  | "outcome_unknown"
  | "side_effect_observed"
  | "pointer_moved"
  | "button_pressed"
  | "button_released"
  | "pointer_clicked"
  | "pointer_dragged"
  | "pointer_scrolled"
  | "key_pressed"
  | "key_released"
  | "keyboard_action_completed"
  | "input_reset"
  | "process_started"
  | "process_signalled"
  | "process_exited"
  | "postcondition_met"
  | "window_request_sent"
  | "window_state_changed"
  | "clipboard_ownership_changed"
  | "text_inserted"
  | "semantic_action_dispatched"
  | "semantic_state_changed"
  | "element_physically_clicked";

export type CleanupStatus =
  | "not_required"
  | "completed"
  | "failed"
  | "partial"
  | "unknown";

export interface ApiProblem {
  readonly type?: string;
  readonly title?: string;
  readonly status?: number;
  readonly detail?: string;
  readonly code?: string;
  readonly request_id?: string;
  readonly command_id?: string;
  readonly retry?: RetryAdvice;
  readonly effect_stage?: EffectStage;
  readonly cleanup?: CleanupStatus;
  readonly details?: JsonObject;
  readonly [key: string]: unknown;
}
