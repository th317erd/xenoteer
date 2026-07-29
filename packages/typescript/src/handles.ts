// SPDX-License-Identifier: Apache-2.0

import type {
  Desktop,
  ElementResolveSpec,
  SubmitOptions,
  WindowResolveSpec,
} from "./desktop.js";
import type { CommandHandle } from "./command.js";
import { XenoteerError } from "./errors.js";
import type {
  CanonicalUInt64,
  ElementSnapshotResult,
  JsonObject,
  WindowSnapshotResult,
} from "./protocol.generated.js";
import type { RequestOptions } from "./transport.js";

const inspectSymbol = Symbol.for("nodejs.util.inspect.custom");

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function staleError(error: unknown): boolean {
  return error instanceof XenoteerError
    && (
      error.code === "stale_reference"
      || error.problemCode === "stale_reference"
      || error.problemCode === "generation_mismatch"
    );
}

/** One immutable exact-window birth plus its opaque lookup authority. */
export class WindowHandle {
  readonly #desktop: Desktop;
  readonly #selector: JsonObject;
  readonly #order: string;
  readonly #referenceToken: string;
  readonly #identity: JsonObject;
  #stale = false;
  #staleReason?: string;

  constructor(
    desktop: Desktop,
    selector: JsonObject,
    order: string,
    entry: JsonObject,
  ) {
    if (
      typeof entry["reference_token"] !== "string"
      || !isObject(entry["snapshot"])
      || !isObject(entry["snapshot"]["ref"])
    ) {
      throw new XenoteerError("invalid_response", "window handle entry is invalid");
    }
    this.#desktop = desktop;
    this.#selector = Object.freeze(structuredClone(selector));
    this.#order = order;
    this.#referenceToken = entry["reference_token"];
    this.#identity = Object.freeze(
      structuredClone(entry["snapshot"]["ref"]),
    ) as JsonObject;
  }

  get stale(): boolean {
    return this.#stale;
  }

  get identity(): JsonObject {
    return this.#identity;
  }

  async snapshot(options: RequestOptions = {}): Promise<WindowSnapshotResult> {
    this.#ensureFresh();
    try {
      return await this.#desktop.windows.snapshot(this.#referenceToken, options);
    } catch (error) {
      if (staleError(error)) this.markStale("server rejected the exact window birth");
      throw error;
    }
  }

  /** Explicitly queries again and returns a new handle; this object never retargets. */
  async relocate(options: RequestOptions = {}): Promise<WindowHandle> {
    return await this.#desktop.windows.one(this.#selector, this.#order, options);
  }

  async activate(
    options: SubmitOptions & {
      readonly switchWorkspace?: boolean;
      readonly fallback?: string;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const {
      switchWorkspace = true,
      fallback = "wm_then_ewmh",
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "window_activate",
      window: this.#identity,
      switch_workspace: switchWorkspace,
      fallback,
    }, submit);
  }

  async close(
    waitFor: JsonObject | null = null,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    return await this.#desktop.submit({
      type: "window_close",
      window: this.#identity,
      wait_for: waitFor,
    }, options);
  }

  async moveResize(
    geometry: JsonObject,
    options: SubmitOptions & {
      readonly relativeTo?: string;
      readonly boundsPolicy?: string;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const {
      relativeTo = "root",
      boundsPolicy = "allow_offscreen",
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "window_move_resize",
      window: this.#identity,
      relative_to: relativeTo,
      geometry,
      bounds_policy: boundsPolicy,
    }, submit);
  }

  async setState(
    state: string,
    desired: boolean,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    return await this.#desktop.submit({
      type: "window_set_state",
      window: this.#identity,
      state,
      desired,
    }, options);
  }

  async minimize(
    desired = true,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    return await this.#desktop.submit({
      type: "window_minimize",
      window: this.#identity,
      desired,
    }, options);
  }

  async moveToWorkspace(
    workspace: number,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    if (!Number.isSafeInteger(workspace) || workspace < 0) {
      throw new XenoteerError("invalid_request", "workspace must be a nonnegative integer");
    }
    return await this.#desktop.submit({
      type: "window_move_to_workspace",
      window: this.#identity,
      workspace,
    }, options);
  }

  async windowStack(
    mode: "raise" | "lower" | "above" | "below",
    sibling: WindowHandle | null = null,
    options: SubmitOptions = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    if (sibling !== null) sibling.#ensureFresh();
    const requiresSibling = mode === "above" || mode === "below";
    if (
      !["raise", "lower", "above", "below"].includes(mode)
      || requiresSibling !== (sibling !== null)
    ) {
      throw new XenoteerError(
        "invalid_request",
        "above/below require a sibling while raise/lower forbid one",
      );
    }
    if (
      sibling !== null
      && (
        sibling.identity["desktop_id"] !== this.#identity["desktop_id"]
        || sibling.identity["desktop_generation"] !== this.#identity["desktop_generation"]
      )
    ) {
      throw new XenoteerError(
        "stale_reference",
        "window stack sibling belongs to another desktop generation",
      );
    }
    return await this.#desktop.submit({
      type: "window_stack",
      window: this.#identity,
      mode,
      sibling: sibling?.identity ?? null,
    }, options);
  }

  async capture(
    options: RequestOptions & {
      readonly format?: "png" | "webp_lossless" | "bmp" | "raw_bgra";
      readonly includeCursor?: boolean;
      readonly drawable?: boolean;
      readonly coordinateSpace?: string;
      readonly maxBytes?: number | null;
    } = {},
  ): ReturnType<Desktop["capture"]["screenshot"]> {
    this.#ensureFresh();
    const {
      format = "png",
      includeCursor = false,
      drawable = false,
      coordinateSpace = "frame",
      maxBytes = null,
      ...requestOptions
    } = options;
    return this.#desktop.capture.screenshot({
      target: drawable
        ? { kind: "window_drawable", window: this.#identity }
        : {
            kind: "window_visible",
            window: this.#identity,
            coordinate_space: coordinateSpace,
          },
      format,
      include_cursor: includeCursor,
      region: null,
      scale: null,
      max_bytes: maxBytes,
    }, requestOptions);
  }

  async waitFor(
    predicate: JsonObject,
    timeoutMs: number,
    options: RequestOptions & { readonly afterRevision?: CanonicalUInt64 | null } = {},
  ): ReturnType<Desktop["windows"]["wait"]> {
    this.#ensureFresh();
    const { afterRevision = null, ...requestOptions } = options;
    return this.#desktop.windows.wait({
      target: { type: "reference", window: this.#identity },
      predicate,
      after_revision: afterRevision,
      timeout_ms: timeoutMs,
    }, requestOptions);
  }

  markStale(reason = "reference invalidated"): void {
    this.#stale = true;
    this.#staleReason = reason;
  }

  toString(): string {
    return `WindowHandle(stale=${this.#stale}, reference=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({ stale: this.#stale, reference: "<redacted>" });
  }

  [inspectSymbol](): string {
    return this.toString();
  }

  #ensureFresh(): void {
    try {
      this.#desktop.assertCurrentReference();
    } catch (error) {
      if (staleError(error)) this.markStale("desktop generation changed");
      throw error;
    }
    if (this.#stale) {
      throw new XenoteerError(
        "stale_reference",
        this.#staleReason ?? "window handle is stale",
      );
    }
  }
}

/** One immutable AT-SPI identity. Refresh never relocates bus paths or app instances. */
export class ElementHandle {
  readonly #desktop: Desktop;
  readonly #selector: JsonObject;
  readonly #identity: JsonObject;
  #stale = false;
  #staleReason?: string;

  constructor(desktop: Desktop, selector: JsonObject, entry: JsonObject) {
    if (
      !isObject(entry["snapshot"])
      || !isObject(entry["snapshot"]["ref"])
    ) {
      throw new XenoteerError("invalid_response", "element handle entry is invalid");
    }
    this.#desktop = desktop;
    this.#selector = Object.freeze(structuredClone(selector));
    this.#identity = Object.freeze(
      structuredClone(entry["snapshot"]["ref"]),
    ) as JsonObject;
  }

  get stale(): boolean {
    return this.#stale;
  }

  get identity(): JsonObject {
    return this.#identity;
  }

  async snapshot(
    expansion?: JsonObject,
    options: RequestOptions = {},
  ): Promise<ElementSnapshotResult> {
    this.#ensureFresh();
    try {
      return await this.#desktop.accessibility.snapshot(
        this.#identity,
        expansion,
        options,
      );
    } catch (error) {
      if (staleError(error)) this.markStale("server rejected the exact AT-SPI identity");
      throw error;
    }
  }

  /** Explicit exact-one query returning a new handle; the old identity is unchanged. */
  async relocate(options: RequestOptions = {}): Promise<ElementHandle> {
    return await this.#desktop.accessibility.one(this.#selector, {}, options);
  }

  async invoke(
    action: JsonObject,
    options: SubmitOptions & {
      readonly allowDisabled?: boolean;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const {
      allowDisabled = false,
      postcondition = null,
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "element_invoke",
      element: this.#identity,
      action,
      allow_disabled: allowDisabled,
      postcondition,
    }, submit);
  }

  async focus(
    options: SubmitOptions & {
      readonly requireWindowFocusCorrelation?: boolean;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const {
      requireWindowFocusCorrelation = true,
      postcondition = null,
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "element_focus",
      element: this.#identity,
      require_window_focus_correlation: requireWindowFocusCorrelation,
      postcondition,
    }, submit);
  }

  async setValue(
    value: number,
    options: SubmitOptions & {
      readonly tolerance?: number | null;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    if (!Number.isFinite(value)) {
      throw new XenoteerError("invalid_request", "element value must be finite");
    }
    const { tolerance = null, postcondition = null, ...submit } = options;
    return await this.#desktop.submit({
      type: "element_set_value",
      element: this.#identity,
      value,
      tolerance,
      postcondition,
    }, submit);
  }

  async setText(
    text: string,
    options: SubmitOptions & {
      readonly selection?: "preserve" | "collapse_before" | "collapse_after" | "select_inserted";
      readonly verifyLengthOnly?: boolean;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    if (new TextEncoder().encode(text).byteLength > 256 * 1024) {
      throw new XenoteerError("request_too_large", "element text exceeds 256 KiB");
    }
    const {
      selection = "preserve",
      verifyLengthOnly = false,
      postcondition = null,
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "element_set_text",
      element: this.#identity,
      text,
      selection,
      verify_length_only: verifyLengthOnly,
      postcondition,
    }, submit);
  }

  async insertText(
    offset: number,
    text: string,
    options: SubmitOptions & {
      readonly selection?: "preserve" | "collapse_before" | "collapse_after" | "select_inserted";
      readonly verifyLengthOnly?: boolean;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > 2_147_483_646) {
      throw new XenoteerError("invalid_request", "element text offset is invalid");
    }
    if (text.includes("\0")) {
      throw new XenoteerError("invalid_request", "element text contains a NUL character");
    }
    if (new TextEncoder().encode(text).byteLength > 256 * 1024) {
      throw new XenoteerError("request_too_large", "element text exceeds its safe input bound");
    }
    const {
      selection = "preserve",
      verifyLengthOnly = false,
      postcondition = null,
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "element_insert_text",
      element: this.#identity,
      offset,
      text,
      selection,
      verify_length_only: verifyLengthOnly,
      postcondition,
    }, submit);
  }

  async select(
    operation: JsonObject,
    options: SubmitOptions & {
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const { postcondition = null, ...submit } = options;
    return await this.#desktop.submit({
      type: "element_selection",
      element: this.#identity,
      operation,
      postcondition,
    }, submit);
  }

  async scroll(
    target: JsonObject,
    options: SubmitOptions & {
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const { postcondition = null, ...submit } = options;
    return await this.#desktop.submit({
      type: "element_scroll",
      element: this.#identity,
      target,
      postcondition,
    }, submit);
  }

  async click(
    window: JsonObject,
    options: SubmitOptions & {
      readonly minimumCorrelation?: string;
      readonly pointPolicy?: JsonObject;
      readonly scrollPolicy?: string;
      readonly activationPolicy?: string;
      readonly occlusionPolicy?: string;
      readonly button?: string;
      readonly count?: number;
      readonly intervalMs?: number;
      readonly moveDurationMs?: number;
      readonly settleTimeoutMs?: number;
      readonly postcondition?: JsonObject | null;
    } = {},
  ): Promise<CommandHandle> {
    this.#ensureFresh();
    const {
      minimumCorrelation = "strong",
      pointPolicy = { type: "center" },
      scrollPolicy = "if_needed",
      activationPolicy = "if_needed",
      occlusionPolicy = "best_effort_reject",
      button = "left",
      count = 1,
      intervalMs = 0,
      moveDurationMs = 250,
      settleTimeoutMs = 3_000,
      postcondition = null,
      ...submit
    } = options;
    return await this.#desktop.submit({
      type: "element_physical_click",
      element: this.#identity,
      window,
      minimum_correlation: minimumCorrelation,
      point_policy: pointPolicy,
      scroll_policy: scrollPolicy,
      activation_policy: activationPolicy,
      occlusion_policy: occlusionPolicy,
      button,
      count,
      interval_ms: intervalMs,
      move_duration_ms: moveDurationMs,
      curve: "smooth",
      settle_timeout_ms: settleTimeoutMs,
      postcondition,
    }, submit);
  }

  async waitFor(
    predicate: JsonObject,
    timeoutMs: number,
    options: RequestOptions & {
      readonly afterRevision?: CanonicalUInt64 | null;
      readonly allowPollFallback?: boolean;
      readonly expansion?: JsonObject;
      readonly limits?: import("./protocol.generated.js").AccessibilityLimits;
    } = {},
  ): ReturnType<Desktop["accessibility"]["wait"]> {
    this.#ensureFresh();
    const {
      afterRevision = null,
      allowPollFallback = false,
      expansion = {},
      limits,
      ...requestOptions
    } = options;
    return this.#desktop.accessibility.wait({
      target: { type: "reference", element: this.#identity },
      predicate,
      after_revision: afterRevision,
      timeout_ms: timeoutMs,
      allow_poll_fallback: allowPollFallback,
      expansion,
      ...(limits === undefined ? {} : { limits }),
    }, requestOptions);
  }

  markStale(reason = "reference invalidated"): void {
    this.#stale = true;
    this.#staleReason = reason;
  }

  toString(): string {
    return `ElementHandle(stale=${this.#stale}, reference=<redacted>)`;
  }

  toJSON(): Readonly<Record<string, unknown>> {
    return Object.freeze({ stale: this.#stale, reference: "<redacted>" });
  }

  [inspectSymbol](): string {
    return this.toString();
  }

  #ensureFresh(): void {
    try {
      this.#desktop.assertCurrentReference();
    } catch (error) {
      if (staleError(error)) this.markStale("desktop generation changed");
      throw error;
    }
    if (this.#stale) {
      throw new XenoteerError(
        "stale_reference",
        this.#staleReason ?? "element handle is stale",
      );
    }
  }
}

export function windowHandleFromResolve(
  desktop: Desktop,
  selector: JsonObject,
  order: string,
  spec: WindowResolveSpec,
  entry: JsonObject,
): WindowHandle {
  if (spec.match_policy !== "exactly_one" && spec.match_policy !== "first") {
    throw new XenoteerError("invalid_request", "window match policy is invalid");
  }
  return new WindowHandle(desktop, selector, order, entry);
}

export function elementHandleFromResolve(
  desktop: Desktop,
  selector: JsonObject,
  _spec: ElementResolveSpec,
  entry: JsonObject,
): ElementHandle {
  return new ElementHandle(desktop, selector, entry);
}
