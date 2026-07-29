#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import { inspect, isDeepStrictEqual } from "node:util";
import {
  BearerToken,
  IssuedViewerTicket,
  XenoteerClient,
  XenoteerError,
  admitRequestVersion,
  classifyCommandEffect,
  decodeAdditiveServerResponse,
  decodeEventMessage,
  decodeUInt64,
  negotiateProtocol,
  validateCommandEnvelopeInput,
} from "../dist/src/index.js";

const INPUT_LIMIT = 8 * 1024 * 1024;
const TOKEN = "conformance-adapter-token-0123456789abcdef";
const FROZEN_CORPUS = "xenoteer-conformance-v1";
const FROZEN_CORPUS_SHA256 =
  "6cc98e72e1de6591cce2d0661f4fc3ea508535d310a40746aa3ad8bd1e61e7fc";
const FROZEN_PROTOCOL = Object.freeze({
  major: 1,
  min_minor: 0,
  max_minor: 0,
});
const UUID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;

async function readInput() {
  const chunks = [];
  let length = 0;
  for await (const chunk of process.stdin) {
    length += chunk.length;
    if (length > INPUT_LIMIT) throw new Error("adapter input exceeds 8 MiB");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function check(condition, label) {
  if (!condition) throw new Error(label);
}

function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Corpus expectations are intentionally partial objects. Arrays are ordered,
 * except additive-forward-compatibility `preserve` pointer lists, which are
 * explicitly sets.
 */
function expectationMatches(actual, expected, property = "") {
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || actual.length !== expected.length) return false;
    if (property === "preserve") {
      const remaining = [...actual];
      for (const expectedItem of expected) {
        const index = remaining.findIndex((item) =>
          expectationMatches(item, expectedItem)
        );
        if (index < 0) return false;
        remaining.splice(index, 1);
      }
      return true;
    }
    return expected.every((item, index) =>
      expectationMatches(actual[index], item)
    );
  }
  if (isObject(expected)) {
    if (!isObject(actual)) return false;
    return Object.entries(expected).every(
      ([key, value]) =>
        Object.hasOwn(actual, key)
        && expectationMatches(actual[key], value, key),
    );
  }
  return isDeepStrictEqual(actual, expected);
}

function pointer(value, path) {
  if (path === "") return value;
  return path.split("/").slice(1).reduce((current, component) => {
    const key = component.replaceAll("~1", "/").replaceAll("~0", "~");
    if (!isObject(current) && !Array.isArray(current)) {
      throw new Error(`JSON pointer ${path} is absent`);
    }
    return current[key];
  }, value);
}

function status(desktopId, generation) {
  return {
    server_version: "0.2.0",
    protocol_min: { major: 1, minor: 0 },
    protocol_max: { major: 1, minor: 0 },
    server_time: "2030-01-01T00:00:00Z",
    desktop: { id: desktopId, generation, state: "ready" },
    capabilities: { capabilities: [] },
  };
}

function json(body, statusCode = 200, type = "application/json") {
  return new Response(JSON.stringify(body), {
    status: statusCode,
    headers: { "content-type": type },
  });
}

async function fixtureResponse(fixture, signal) {
  check(isObject(fixture) && typeof fixture.kind === "string", "response fixture is invalid");
  if (fixture.kind === "disconnect") throw new Error("fixture transport disconnected");
  if (fixture.kind === "stall") {
    check(Number.isSafeInteger(fixture.delay_ms) && fixture.delay_ms >= 0, "stall fixture is invalid");
    await new Promise((resolve, reject) => {
      const timer = setTimeout(resolve, fixture.delay_ms);
      const abort = () => {
        clearTimeout(timer);
        reject(new DOMException("aborted", "AbortError"));
      };
      if (signal?.aborted) abort();
      else signal?.addEventListener("abort", abort, { once: true });
    });
    throw new Error("fixture stalled response disconnected");
  }
  check(
    (fixture.kind === "json" || fixture.kind === "problem")
      && Number.isSafeInteger(fixture.status)
      && isObject(fixture.body),
    "HTTP response fixture is invalid",
  );
  return json(
    fixture.body,
    fixture.status,
    fixture.kind === "problem" ? "application/problem+json" : "application/json",
  );
}

function evaluateNegotiation(input) {
  try {
    return {
      outcome: "accepted",
      selected: negotiateProtocol(
        {
          major: input.client.major,
          minMinor: input.client.min_minor,
          maxMinor: input.client.max_minor,
        },
        { major: input.server.major, minor: input.server.min_minor },
        { major: input.server.major, minor: input.server.max_minor },
      ),
    };
  } catch (error) {
    check(error instanceof XenoteerError, "negotiation escaped the SDK error model");
    return {
      outcome: "rejected",
      code: error.code,
      sdk_error_observed: true,
    };
  }
}

function evaluateRequestVersion(input) {
  try {
    admitRequestVersion(input.negotiated, input.request);
    return { outcome: "accepted" };
  } catch (error) {
    check(error instanceof XenoteerError, "request version escaped the SDK error model");
    return {
      outcome: "rejected",
      code: error.code,
      sdk_error_observed: true,
    };
  }
}

function evaluateUInt64(input) {
  try {
    const value = decodeUInt64(input.wire, { allowZero: input.allow_zero });
    return { outcome: "accepted", decimal: value.toString(10) };
  } catch {
    return { outcome: "rejected", code: "invalid_uint64_string" };
  }
}

function evaluateRequestDecode(input) {
  try {
    validateCommandEnvelopeInput(input.wire);
    return { outcome: "accepted", connection: "usable", preserve: [] };
  } catch (error) {
    return {
      outcome: "rejected",
      code: error instanceof XenoteerError ? error.code : "invalid_request",
      connection: "usable",
      preserve: [],
    };
  }
}

function statusPreservationPaths(wire) {
  const paths = [];
  const top = new Set([
    "server_version",
    "protocol_min",
    "protocol_max",
    "server_time",
    "desktop",
    "capabilities",
  ]);
  for (const key of Object.keys(wire)) {
    if (!top.has(key)) paths.push(`/${key}`);
  }
  for (const [name, known] of [
    ["protocol_min", new Set(["major", "minor"])],
    ["protocol_max", new Set(["major", "minor"])],
    ["desktop", new Set(["id", "generation", "state"])],
  ]) {
    if (!isObject(wire[name])) continue;
    for (const key of Object.keys(wire[name])) {
      if (!known.has(key)) paths.push(`/${name}/${key}`);
    }
  }
  return paths.sort();
}

function evaluateResponseDecode(input) {
  const wire = input.wire;
  try {
    const decoded = decodeAdditiveServerResponse(wire);
    const preserve = decoded.kind === "unknown_message"
      ? [""]
      : decoded.kind === "status"
        ? statusPreservationPaths(wire)
        : [];
    for (const path of preserve) {
      check(isDeepStrictEqual(pointer(decoded.raw, path), pointer(wire, path)), `response lost ${path}`);
    }
    return {
      outcome: decoded.kind === "unknown_message" ? "unknown_message" : "accepted",
      ...(decoded.kind === "unknown_message" ? { connection: "usable" } : { known_type: decoded.kind }),
      preserve,
    };
  } catch (error) {
    if (!(error instanceof XenoteerError)) throw error;
    const preserve = [];
    if (
      error.code === "unsupported_response_variant"
      && isObject(wire?.result?.outcome)
    ) {
      check(error.details?.outcome === "<redacted>", "unknown outcome was not safely retained");
      check(
        error.details?.outcome_type === wire.result.outcome.type,
        "unknown outcome type was not retained",
      );
      preserve.push("/result/outcome");
    }
    return {
      outcome: "operation_error",
      code: error.code,
      connection: "usable",
      preserve,
    };
  }
}

function evaluateEventDecode(input) {
  const decoded = decodeEventMessage(input.wire);
  check(decoded.kind === "unknown", "future event did not remain observable");
  const preserve = ["/event/payload", "/event/sequence", "/event/topic"];
  for (const path of preserve) {
    check(isDeepStrictEqual(pointer(decoded.raw, path), pointer(input.wire, path)), `event lost ${path}`);
  }
  return { outcome: "unknown_event", preserve };
}

function normalizeEnvelope(encoded, commandId) {
  const envelope = JSON.parse(encoded);
  check(envelope.command_id === commandId, "submission command ID changed");
  return {
    protocol_version: envelope.protocol_version,
    request_id_non_nil: typeof envelope.request_id === "string"
      && UUID.test(envelope.request_id)
      && envelope.request_id !== "00000000-0000-0000-0000-000000000000",
    command_id: envelope.command_id,
    desktop_id: envelope.desktop_id,
    desktop_generation: envelope.desktop_generation,
    lease_id: envelope.lease_id,
    deadline: envelope.deadline,
    trace_policy: envelope.trace_policy,
    command: envelope.command,
  };
}

async function evaluateCommandReconnect(input) {
  check(
    typeof input.desktop_id === "string"
      && typeof input.desktop_generation === "string"
      && typeof input.reconnect_generation === "string"
      && typeof input.command_id === "string"
      && isObject(input.command),
    "command reconnect input is invalid",
  );
  let generation = input.desktop_generation;
  let submissions = 0;
  let lookups = 0;
  let cancellations = 0;
  const encoded = [];
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (request, init = {}) => {
      const url = String(request);
      if (url.endsWith("/v1/status")) {
        return json(status(input.desktop_id, generation));
      }
      if (init.method === "POST" && url.endsWith("/commands")) {
        const fixture = submissions === 0 ? input.initial_response : input.resubmit_response;
        submissions += 1;
        const body = String(init.body);
        encoded.push(body);
        check(
          new Headers(init.headers).get("idempotency-key") === input.command_id,
          "submission omitted exact idempotency key",
        );
        return await fixtureResponse(fixture, init.signal);
      }
      if (init.method === "DELETE" && url.includes(`/commands/${input.command_id}`)) {
        cancellations += 1;
        throw new Error("adapter must never implicitly cancel server work");
      }
      if (init.method === "GET" && url.includes(`/commands/${input.command_id}`)) {
        lookups += 1;
        return await fixtureResponse(input.lookup_response, init.signal);
      }
      throw new Error(`unexpected command request: ${init.method ?? "GET"} ${url}`);
    },
  });
  const desktop = client.desktop();
  const submission = desktop.prepareSubmission(input.command, {
    commandId: input.command_id,
    tracePolicy: null,
  });
  const controller = input.cancel_after_ms === null
    ? undefined
    : new AbortController();
  const timer = controller === undefined
    ? undefined
    : setTimeout(() => controller.abort(), input.cancel_after_ms);
  try {
    await submission.send(controller === undefined ? {} : { signal: controller.signal });
  } catch {
    // Transport ambiguity and local await cancellation both require reconciliation.
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }

  let outcome = null;
  let lifecycle = null;
  let errorCode = null;
  if (input.reconnect_generation !== input.desktop_generation) {
    generation = input.reconnect_generation;
    await client.refreshStatus();
    try {
      await submission.send();
      throw new Error("stale submission unexpectedly crossed a generation boundary");
    } catch (error) {
      check(
        error instanceof XenoteerError
          && error.code === "generation_changed"
          && submissions === 1,
        "stale generation did not fail before I/O",
      );
      errorCode = error.code;
    }
    outcome = "stale_generation";
  } else {
    let lookupHandle;
    if (input.lookup_response !== null) {
      try {
        lookupHandle = await desktop.command(input.command_id);
      } catch (error) {
        check(
          error instanceof XenoteerError && error.problemCode === "not_found",
          "lookup failed with an unexpected error",
        );
      }
    }
    if (lookupHandle !== undefined) {
      outcome = "reattached";
      lifecycle = lookupHandle.latest.lifecycle;
    } else if (input.resubmit_command !== null) {
      const retry = isDeepStrictEqual(input.resubmit_command, input.command)
        ? submission
        : desktop.prepareSubmission(input.resubmit_command, {
            commandId: input.command_id,
            tracePolicy: null,
          });
      try {
        const handle = await retry.send();
        outcome = "resubmitted";
        lifecycle = handle.latest.lifecycle;
      } catch (error) {
        check(error instanceof XenoteerError, "resubmission failed outside the SDK error model");
        errorCode = error.problemCode ?? error.code;
        outcome = errorCode;
      }
    } else {
      throw new Error("ambiguous command had neither lookup nor resubmission path");
    }
  }
  if (
    encoded.length > 1
    && isDeepStrictEqual(input.command, input.resubmit_command)
  ) {
    check(encoded[0] === encoded[1], "same-command retry changed serialized request bytes");
  }
  return {
    outcome,
    command_id: input.command_id,
    submission_attempts: submissions,
    lookup_attempts: lookups,
    cancel_requests: cancellations,
    submission_envelopes: encoded.map((body) => normalizeEnvelope(body, input.command_id)),
    lifecycle,
    error_code: errorCode,
  };
}

async function evaluateEffect(input) {
  check(isObject(input.result) && typeof input.result.command_id === "string", "effect result is invalid");
  const result = input.result;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (request) => {
      const url = String(request);
      if (url.endsWith("/v1/status")) {
        return json(status(
          "20000000-0000-4000-8000-000000000001",
          "30000000-0000-4000-8000-000000000001",
        ));
      }
      if (url.includes(`/commands/${result.command_id}`)) return json(result);
      throw new Error(`unexpected effect request: ${url}`);
    },
  });
  const handle = await client.desktop().command(result.command_id);
  const latest = handle.latest;
  const classification = classifyCommandEffect(latest);
  return {
    lifecycle: latest.lifecycle,
    effect_stage: latest.effect_stage,
    has_visible_effect: classification.afterEffect,
    error_code: latest.error?.code ?? null,
    retry: latest.error?.retry ?? null,
    details: latest.error?.details ?? {},
    warning_count: latest.warnings.length,
    outcome_type: latest.outcome?.type ?? null,
  };
}

class FakeSocket {
  readyState = 0;
  sent = [];
  #listeners = new Map();
  #respond;

  constructor(respond) {
    this.#respond = respond;
    queueMicrotask(() => {
      this.readyState = 1;
      this.#emit("open", {});
    });
  }

  addEventListener(type, listener, options = {}) {
    const wrapped = options.once === true
      ? (event) => {
          this.#listeners.set(
            type,
            (this.#listeners.get(type) ?? []).filter((item) => item !== wrapped),
          );
          listener(event);
        }
      : listener;
    this.#listeners.set(type, [...(this.#listeners.get(type) ?? []), wrapped]);
  }

  send(data) {
    this.sent.push(data);
    this.#respond(this, JSON.parse(data));
  }

  close(code = 1000, reason = "") {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.#emit("close", { code, reason });
  }

  async message(value) {
    this.#emit("message", { data: JSON.stringify(value) });
    // EventSession intentionally detaches its async message handler. Drain the
    // fixed decode/dispatch microtask chain before the fixture sends another
    // frame, without introducing wall-clock sleeps or load-sensitive races.
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
  }

  #emit(type, event) {
    for (const listener of [...(this.#listeners.get(type) ?? [])]) listener(event);
  }
}

function welcome(input) {
  return {
    type: "server.welcome",
    protocol: { major: 1, minor: 0 },
    connection_id: "70000000-0000-4000-8000-000000000001",
    principal: { id: "conformance", capabilities: [] },
    desktop: {
      id: input.desktop_id,
      generation: input.desktop_generation,
      state: "ready",
    },
    limits: {
      max_message_bytes: 1_048_576,
      heartbeat_ms: 60_000,
      normal_outbound_capacity: 16,
      reserved_outbound_capacity: 4,
      max_command_watches: 16,
    },
    resume: { status: "not_requested" },
  };
}

async function evaluateEventContinuity(input) {
  check(
    typeof input.desktop_id === "string"
      && typeof input.desktop_generation === "string"
      && Array.isArray(input.topics)
      && Array.isArray(input.frames)
      && Number.isSafeInteger(input.queue_capacity),
    "event continuity input is invalid",
  );
  let socket;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status(input.desktop_id, input.desktop_generation)),
  });
  const session = await client.openEventSession(() => {
    socket = new FakeSocket((current, message) => {
      if (message.type === "client.hello") {
        queueMicrotask(() => void current.message(welcome(input)));
      } else if (message.type === "events.subscribe") {
        check(
          message.request_id === input.subscription_request_id,
          "SDK subscription request ID differed from fixture input",
        );
        check(
          message.desktop_id === input.desktop_id
            && message.desktop_generation === input.desktop_generation
            && isDeepStrictEqual(message.topics, input.topics)
            && message.since_sequence === input.initial_cursor,
          "SDK subscription scope differed from fixture input",
        );
        queueMicrotask(() => void current.message({
          type: "events.subscribed",
          request_id: message.request_id,
          topics: message.topics,
        }));
      } else if (message.type === "client.ping") {
        queueMicrotask(() => void current.message({
          type: "server.pong",
          request_id: message.request_id,
          nonce: message.nonce,
        }));
      }
    });
    return socket;
  }, {
    capacity: input.queue_capacity,
    reconnect: { maxAttempts: 0 },
    handshakeTimeoutMs: 1_000,
    acknowledgmentTimeoutMs: 1_000,
  });
  check(
    typeof input.subscription_request_id === "string"
      && UUID.test(input.subscription_request_id)
      && input.subscription_request_id !== "00000000-0000-0000-0000-000000000000",
    "event subscription request ID is invalid",
  );
  const originalRandomUuid = globalThis.crypto.randomUUID;
  globalThis.crypto.randomUUID = () => input.subscription_request_id;
  try {
    await session.subscribe(input.topics, input.initial_cursor);
  } finally {
    globalThis.crypto.randomUUID = originalRandomUuid;
  }
  for (const frame of input.frames) await socket.message(frame);

  const iterator = session[Symbol.asyncIterator]();
  const delivered = [];
  let resyncReason = null;
  const terminalCode = session.terminalReason?.code ?? null;
  if (terminalCode === null) await session.close();
  for (let index = 0; index <= input.frames.length; index += 1) {
    const next = await iterator.next();
    if (next.done) break;
    if (next.value.kind === "known" || next.value.kind === "unknown") {
      delivered.push(next.value.sequence.toString(10));
    } else if (next.value.kind === "resync_required") {
      resyncReason = next.value.reason;
    } else if (next.value.kind === "replay_complete") {
      continue;
    }
  }
  let generationChanged = false;
  try {
    client.desktop();
  } catch (error) {
    check(error instanceof XenoteerError, "generation fence escaped the SDK error model");
    generationChanged = error.code === "stale_reference";
  }
  return {
    delivered_sequences: delivered,
    final_cursor: session.lastSequence?.toString(10) ?? input.initial_cursor,
    terminal: terminalCode === "backpressure" ? "queue_overflow" : terminalCode,
    resync_reason: resyncReason,
    refresh_required:
      terminalCode === "backpressure"
      || terminalCode === "invalid_message"
      || terminalCode === "resync_required",
    generation_changed: generationChanged,
  };
}

function windowEntry(reference, token) {
  return {
    reference_token: token,
    snapshot: { ref: reference, model_revision: reference.observed_generation, warnings: [] },
  };
}

function elementEntry(reference) {
  return {
    snapshot: {
      ref: reference,
      revision: reference.cache_sequence,
      role: {},
      states: [],
      interfaces: [],
      actions: [],
      attributes: [],
      relations: [],
      window_correlation: {},
      completeness: "complete",
      truncated: false,
      warnings: [],
    },
  };
}

function referenceResolve(input, reference, ordinal) {
  if (input.kind === "window") {
    return {
      desktop_id: input.desktop_id ?? reference.desktop_id,
      desktop_generation: reference.desktop_generation,
      snapshot_revision: reference.observed_generation,
      window: windowEntry(reference, `fixture-window-token-${String(ordinal).padStart(4, "0")}`),
    };
  }
  return {
    desktop_id: reference.desktop_id,
    desktop_generation: reference.desktop_generation,
    atspi_generation: reference.atspi_generation,
    snapshot_revision: reference.cache_sequence,
    element: elementEntry(reference),
  };
}

function referenceGenerationChanged(kind, before, after) {
  if (kind === "window") {
    return before.desktop_generation !== after.desktop_generation
      || before.observed_generation !== after.observed_generation
      || before.identity_hash !== after.identity_hash;
  }
  return before.desktop_generation !== after.desktop_generation
    || before.atspi_generation !== after.atspi_generation
    || before.application?.app_instance_generation !== after.application?.app_instance_generation
    || before.application?.identity_hash !== after.application?.identity_hash
    || before.object_identity_hash !== after.object_identity_hash;
}

async function evaluateReference(input) {
  check(
    (input.kind === "window" || input.kind === "element")
      && isObject(input.original)
      && isObject(input.current)
      && isObject(input.server_problem),
    "reference lifecycle input is invalid",
  );
  const resolveValues = [input.original, input.current];
  if (input.relocated !== null) resolveValues.push(input.relocated);
  let resolveIndex = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (request) => {
      const url = String(request);
      if (url.endsWith("/v1/status")) {
        return json(status(input.original.desktop_id, input.original.desktop_generation));
      }
      const isSnapshot = input.kind === "window"
        ? url.includes("/windows/") && !url.endsWith("/windows/resolve")
        : url.endsWith("/accessibility/elements/snapshot");
      if (isSnapshot) {
        return json(input.server_problem, input.server_problem.status, "application/problem+json");
      }
      if (url.endsWith("/windows/resolve") || url.endsWith("/accessibility/elements/resolve")) {
        const value = resolveValues[resolveIndex];
        check(value !== undefined, "reference fixture exhausted");
        resolveIndex += 1;
        return json(referenceResolve(input, value, resolveIndex));
      }
      throw new Error(`unexpected reference request: ${url}`);
    },
  });
  const desktop = client.desktop();
  const selector = input.kind === "window"
    ? { type: "predicate", predicate: { type: "active", value: true } }
    : { scope: { type: "desktop" }, predicates: [], order: "preorder" };
  const original = input.kind === "window"
    ? await desktop.windows.one(selector, "creation_ascending")
    : await desktop.accessibility.one(selector);
  const originalIdentity = structuredClone(original.identity);
  let serverErrorCode = null;
  try {
    await original.snapshot();
  } catch (error) {
    check(error instanceof XenoteerError, "reference snapshot escaped the SDK error model");
    serverErrorCode = error.problemCode ?? error.code;
  }
  const current = input.kind === "window"
    ? await desktop.windows.one(selector, "creation_ascending")
    : await desktop.accessibility.one(selector);
  let relocatedDistinct = false;
  if (input.relocated !== null) {
    const relocated = await original.relocate();
    relocatedDistinct = relocated !== original
      && !isDeepStrictEqual(relocated.identity, original.identity);
  }
  return {
    stale: original.stale,
    server_error_code: serverErrorCode,
    identity_unchanged: isDeepStrictEqual(original.identity, originalIdentity),
    relocated_distinct: relocatedDistinct,
    generation_changed: referenceGenerationChanged(
      input.kind,
      original.identity,
      current.identity,
    ),
  };
}

function diagnosticSurfaces(value) {
  return [String(value), JSON.stringify(value), inspect(value, { depth: 8 })];
}

async function evaluateRedaction(input) {
  check(
    typeof input.kind === "string"
      && typeof input.secret === "string"
      && typeof input.base_url === "string"
      && isObject(input.raw),
    "redaction input is invalid",
  );
  const rawContainsSecret = input.kind === "bearer"
    ? input.raw.authorization === `Bearer ${input.secret}`
    : input.kind === "viewer"
      ? input.raw.ticket?.ticket === input.secret
      : JSON.stringify(input.raw).includes(input.secret);
  check(rawContainsSecret, "raw redaction fixture does not carry its declared secret");

  const urls = [];
  const errors = [];
  const debug = [];
  if (input.kind === "bearer") {
    const bearerValue = input.raw.authorization.slice("Bearer ".length);
    try {
      const bearer = new BearerToken(bearerValue);
      debug.push(...diagnosticSurfaces(bearer));
    } catch (error) {
      errors.push(...diagnosticSurfaces(error));
    }
    try {
      await XenoteerClient.connect({
        baseUrl: input.base_url,
        token: bearerValue,
        fetch: async (request, init = {}) => {
          urls.push(String(request));
          check(
            new Headers(init.headers).get("authorization")
              === input.raw.authorization,
            "bearer fixture did not reach the Authorization header",
          );
          throw new Error(`deterministic bearer failure contains ${input.secret}`);
        },
      });
      throw new Error("bearer failure fixture unexpectedly connected");
    } catch (error) {
      errors.push(...diagnosticSurfaces(error));
    }
    check(debug.length > 0, "bearer fixture did not exercise SDK diagnostics");
    check(errors.length > 0, "bearer fixture did not exercise an SDK error surface");
    check(urls.length > 0, "bearer fixture did not exercise an SDK URL surface");
    return {
      debug_leaked: debug.some((surface) => surface.includes(input.secret)),
      error_leaked: errors.some((surface) => surface.includes(input.secret)),
      url_leaked: urls.some((surface) => surface.includes(input.secret)),
      failure_surface_exercised: errors.length > 0,
      url_surface_observed: urls.length > 0,
    };
  }
  const client = await XenoteerClient.connect({
    baseUrl: input.base_url,
    token: TOKEN,
    fetch: async (request) => {
      const url = String(request);
      urls.push(url);
      if (url.endsWith("/v1/status")) {
        return json(status(
          "20000000-0000-4000-8000-000000000001",
          "30000000-0000-4000-8000-000000000001",
        ));
      }
      throw new Error(`fixture transport contains ${input.secret}`);
    },
  });
  const desktop = client.desktop();
  try {
    if (input.kind === "artifact") {
      const bytes = new TextEncoder().encode(input.raw.bytes_utf8);
      await desktop.artifacts.uploadClipboardInput(bytes, input.raw.content_type);
    } else if (input.kind === "command" || input.kind === "clipboard") {
      const submission = desktop.prepareSubmission(input.raw.command);
      debug.push(...diagnosticSurfaces(submission));
      await submission.send();
    } else if (input.kind === "viewer") {
      const ticket = new IssuedViewerTicket(input.raw.ticket);
      debug.push(...diagnosticSurfaces(ticket));
      urls.push(desktop.viewer.pageUrl());
      await desktop.windows.list();
    } else {
      throw new Error(`unknown redaction kind: ${input.kind}`);
    }
  } catch (error) {
    errors.push(...diagnosticSurfaces(error));
  }
  check(errors.length > 0, "redaction fixture did not exercise an SDK error surface");
  return {
    debug_leaked: debug.some((surface) => surface.includes(input.secret)),
    error_leaked: errors.some((surface) => surface.includes(input.secret)),
    url_leaked: urls.some((surface) => surface.includes(input.secret)),
  };
}

async function evaluate(operation, input) {
  switch (operation) {
    case "negotiate_protocol_range":
      return evaluateNegotiation(input);
    case "admit_request_version":
      return evaluateRequestVersion(input);
    case "decode_uint64_string":
      return evaluateUInt64(input);
    case "decode_request":
      return evaluateRequestDecode(input);
    case "decode_response":
      return evaluateResponseDecode(input);
    case "decode_event":
      return evaluateEventDecode(input);
    case "command_reconnect":
      return await evaluateCommandReconnect(input);
    case "classify_terminal_effect":
      return await evaluateEffect(input);
    case "event_continuity":
      return await evaluateEventContinuity(input);
    case "reference_lifecycle":
      return await evaluateReference(input);
    case "redaction":
      return await evaluateRedaction(input);
    default:
      throw new Error(`unsupported conformance operation: ${operation}`);
  }
}

async function main() {
  const payload = await readInput();
  if (
    !isObject(payload)
    || payload.adapter_protocol !== 1
    || !Array.isArray(payload.cases)
    || payload.cases.length < 1
  ) {
    throw new Error("adapter payload is invalid");
  }
  if (
    payload.corpus !== FROZEN_CORPUS
    || payload.corpus_sha256 !== FROZEN_CORPUS_SHA256
    || !isDeepStrictEqual(payload.protocol, FROZEN_PROTOCOL)
  ) {
    throw new Error("adapter payload does not match frozen conformance identity");
  }
  const results = [];
  for (const testCase of payload.cases) {
    try {
      check(
        isObject(testCase)
          && typeof testCase.id === "string"
          && typeof testCase.operation === "string"
          && isObject(testCase.input)
          && isObject(testCase.expect),
        "conformance case is invalid",
      );
      const actual = await evaluate(testCase.operation, testCase.input);
      check(
        expectationMatches(actual, testCase.expect),
        `actual ${JSON.stringify(actual)} did not satisfy ${JSON.stringify(testCase.expect)}`,
      );
      results.push({
        detail: "public SDK behavior satisfied concrete corpus fixture",
        id: testCase.id,
        status: "passed",
      });
    } catch (error) {
      results.push({
        detail: error instanceof Error ? error.message : "unknown adapter failure",
        id: isObject(testCase) && typeof testCase.id === "string"
          ? testCase.id
          : "<invalid>",
        status: "failed",
      });
    }
  }
  process.stdout.write(`${JSON.stringify({ adapter_protocol: 1, results })}\n`);
}

main().catch((error) => {
  process.stderr.write(
    `TypeScript conformance adapter failed: ${error instanceof Error ? error.message : "unknown error"}\n`,
  );
  process.exitCode = 2;
});
