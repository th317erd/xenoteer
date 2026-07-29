// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import test from "node:test";

import {
  admitRequestVersion,
  BearerToken,
  decodeAdditiveServerResponse,
  XenoteerClient,
  XenoteerError,
  decodeEventMessage,
  decodeUInt64,
  encodeUInt64,
  negotiateProtocol,
  redactedClientOptions,
} from "../src/index.js";

const TOKEN = "sdk-secret-canary-0123456789abcdef";
const DESKTOP_ID = "20000000-0000-4000-8000-000000000001";
const GENERATION = "30000000-0000-4000-8000-000000000001";
const LEASE_ID = "40000000-0000-4000-8000-000000000001";
const COMMAND_ID = "50000000-0000-4000-8000-000000000001";

function status(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    server_version: "0.2.0",
    protocol_min: { major: 1, minor: 0, future_floor: "stable" },
    protocol_max: { major: 1, minor: 0 },
    server_time: "2030-01-01T00:00:00Z",
    desktop: {
      id: DESKTOP_ID,
      generation: GENERATION,
      state: "ready",
      future_health: { score: 100 },
    },
    capabilities: { capabilities: [] },
    ...overrides,
  };
}

function jsonResponse(
  body: unknown,
  init: { readonly status?: number; readonly contentType?: string } = {},
): Response {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: { "content-type": init.contentType ?? "application/json" },
  });
}

function commandResult(
  lifecycle: "accepted" | "running" | "succeeded" = "accepted",
): Record<string, unknown> {
  return {
    command_id: COMMAND_ID,
    lifecycle,
    effect_stage: lifecycle === "succeeded" ? "pointer_moved" : "accepted",
    accepted_at: "2030-01-01T00:00:00Z",
    warnings: [],
    ...(lifecycle === "succeeded"
      ? {
          started_at: "2030-01-01T00:00:00Z",
          finished_at: "2030-01-01T00:00:01Z",
          outcome: { type: "acknowledged" },
          error: null,
        }
      : {}),
  };
}

test("connect negotiates v1 and preserves additive status fields", async () => {
  const calls: Array<{ url: string; init: RequestInit }> = [];
  const mockFetch: typeof fetch = async (input, init = {}) => {
    calls.push({ url: String(input), init });
    return jsonResponse({
      ...status(),
      future_server_metadata: { build_channel: "community" },
    });
  };

  const client = await XenoteerClient.connect({
    baseUrl: "https://127.0.0.1:9443",
    token: TOKEN,
    fetch: mockFetch,
  });
  assert.deepEqual(client.negotiatedProtocol, { major: 1, minor: 0 });
  assert.deepEqual(client.status["future_server_metadata"], {
    build_channel: "community",
  });
  assert.deepEqual(client.status.desktop["future_health"], { score: 100 });
  assert.equal(client.desktop().generation, GENERATION);
  assert.equal(calls.length, 1);
  assert.equal(calls[0]?.url, "https://127.0.0.1:9443/v1/status");
  const headers = new Headers(calls[0]?.init.headers);
  assert.equal(headers.get("authorization"), `Bearer ${TOKEN}`);
  assert.equal(headers.get("accept"), "application/json, application/problem+json");
});

test("protocol negotiation chooses highest common minor and rejects disjoint ranges", () => {
  assert.deepEqual(
    negotiateProtocol(
      { major: 1, minMinor: 0, maxMinor: 5 },
      { major: 1, minor: 2 },
      { major: 1, minor: 3 },
    ),
    { major: 1, minor: 3 },
  );
  assert.throws(
    () => negotiateProtocol(
      { major: 1, minMinor: 0, maxMinor: 1 },
      { major: 1, minor: 2 },
      { major: 1, minor: 4 },
    ),
    (error: unknown) => error instanceof XenoteerError
      && error.code === "no_shared_minor",
  );
});

test("public protocol helpers expose exact negotiation rejection codes", () => {
  const rejectedWith = (
    expected: string,
    operation: () => unknown,
  ): void => {
    assert.throws(
      operation,
      (error: unknown) => error instanceof XenoteerError
        && error.code === expected,
    );
  };
  rejectedWith("reversed_minor_range", () => negotiateProtocol(
    { major: 1, minMinor: 2, maxMinor: 1 },
    { major: 1, minor: 0 },
    { major: 1, minor: 3 },
  ));
  rejectedWith("reversed_minor_range", () => negotiateProtocol(
    { major: 1, minMinor: 0, maxMinor: 3 },
    { major: 1, minor: 4 },
    { major: 1, minor: 3 },
  ));
  rejectedWith("unsupported_major", () => negotiateProtocol(
    { major: 2, minMinor: 0, maxMinor: 0 },
    { major: 1, minor: 0 },
    { major: 1, minor: 9 },
  ));
  rejectedWith("no_shared_minor", () => negotiateProtocol(
    { major: 1, minMinor: 0, maxMinor: 1 },
    { major: 1, minor: 2 },
    { major: 1, minor: 4 },
  ));
  rejectedWith("unsupported_version", () => admitRequestVersion(
    { major: 1, minor: 2 },
    { major: 1, minor: 3 },
  ));
  rejectedWith("invalid_request", () => negotiateProtocol(
    null as unknown as import("../src/index.js").ProtocolRange,
    { major: 1, minor: 0 },
    { major: 1, minor: 0 },
  ));
});

test("status rejects malformed known capability and reason-code fields", async () => {
  const valid = status({
    desktop: {
      id: DESKTOP_ID,
      generation: GENERATION,
      state: "degraded",
      reason_code: "optional.backend_unavailable",
    },
    capabilities: {
      capabilities: [{
        id: "input.pointer.smooth",
        status: "degraded",
        reason_code: "backend.partial",
        backend_version: "1.2.3",
        future_probe_metadata: { retained: true },
      }],
    },
  });
  const accepted = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => jsonResponse(valid),
  });
  assert.deepEqual(
    (
      accepted.status.capabilities.capabilities[0] as
        Record<string, unknown>
    )["future_probe_metadata"],
    { retained: true },
  );
  await accepted.close();

  const invalidStatuses = [
    status({
      desktop: {
        id: DESKTOP_ID,
        generation: GENERATION,
        state: "ready",
        reason_code: "contains a space",
      },
    }),
    status({ capabilities: { capabilities: [null] } }),
    status({
      capabilities: {
        capabilities: [{ id: "input.pointer.smooth" }],
      },
    }),
    status({
      capabilities: {
        capabilities: [{ id: "input.pointer.smooth", status: "warming" }],
      },
    }),
    status({
      capabilities: {
        capabilities: [{ id: "Input.*", status: "available" }],
      },
    }),
    status({
      capabilities: {
        capabilities: [{
          id: "input.pointer.smooth",
          status: "unavailable",
          reason_code: "bad reason",
        }],
      },
    }),
    status({
      capabilities: {
        capabilities: [{
          id: "input.pointer.smooth",
          status: "available",
          backend_version: "",
        }],
      },
    }),
    status({
      capabilities: {
        capabilities: Array.from(
          { length: 257 },
          (_, index) => ({
            id: `fixture.capability_${index}`,
            status: "available",
          }),
        ),
      },
    }),
    status({
      protocol_min: { major: 1, minor: 2 },
      protocol_max: { major: 1, minor: 1 },
    }),
  ];
  for (const invalid of invalidStatuses) {
    assert.throws(
      () => decodeAdditiveServerResponse(invalid),
      { code: "invalid_response" },
    );
    await assert.rejects(
      () => XenoteerClient.connect({
        baseUrl: "http://127.0.0.1:8080",
        token: TOKEN,
        fetch: async () => jsonResponse(invalid),
      }),
      { code: "invalid_response" },
    );
  }
});

test("token and diagnostic option projections are redacted", () => {
  const token = new BearerToken(TOKEN);
  const safe = redactedClientOptions({
    baseUrl: "https://127.0.0.1:9443",
    token: TOKEN,
  });
  for (const rendered of [
    String(token),
    JSON.stringify(token),
    JSON.stringify(safe),
  ]) {
    assert.equal(rendered.includes(TOKEN), false);
    assert.equal(rendered.includes("redacted"), true);
  }
});

test("canonical uint64 values never pass through number", () => {
  assert.equal(decodeUInt64("9007199254740993"), 9_007_199_254_740_993n);
  assert.equal(
    encodeUInt64(18_446_744_073_709_551_615n),
    "18446744073709551615",
  );
  for (const invalid of [
    "",
    "+1",
    "01",
    "-1",
    "١",
    1,
    null,
    "18446744073709551616",
    "1 ",
  ]) {
    assert.throws(() => decodeUInt64(invalid));
  }
});

test("unknown event topic preserves exact immutable raw payload and sequence", () => {
  const raw = {
    type: "event",
    request_id: "10000000-0000-4000-8000-000000000001",
    event: {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      sequence: "9007199254740993",
      topic: "future.widget.changed",
      payload: {
        future_flag: true,
        revision: "18446744073709551615",
      },
    },
  };
  const event = decodeEventMessage(raw);
  assert.equal(event.kind, "unknown");
  assert.equal(event.sequence, 9_007_199_254_740_993n);
  assert.equal(event.raw.event.sequence, "9007199254740993");
  assert.deepEqual(event.raw.event.payload, raw.event.payload);
  assert.equal(Object.isFrozen(event.raw), true);
  assert.equal(Object.isFrozen(event.raw.event.payload), true);
});

test("controlled commands use one attempt, smooth motion, explicit renew/release", async () => {
  const calls: Array<{ url: string; init: RequestInit; body: unknown }> = [];
  const replies = [
    status(),
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      state: "held_by_caller",
      lease_id: LEASE_ID,
      expires_at: "2030-01-01T00:00:30Z",
    },
    commandResult("accepted"),
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      state: "held_by_caller",
      lease_id: LEASE_ID,
      expires_at: "2030-01-01T00:01:00Z",
    },
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      state: "vacant",
      lease_id: null,
      expires_at: null,
    },
  ];
  const mockFetch: typeof fetch = async (input, init = {}) => {
    const rawBody = typeof init.body === "string" ? JSON.parse(init.body) as unknown : undefined;
    calls.push({ url: String(input), init, body: rawBody });
    const reply = replies.shift();
    assert.notEqual(reply, undefined);
    return jsonResponse(reply);
  };
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: mockFetch,
  });
  const lease = await client.desktop().acquireControl(30_000);
  const handle = await lease.mouse.move(120, 300, {
    commandId: COMMAND_ID,
    durationMs: 350,
  });
  assert.equal(handle.id, COMMAND_ID);
  const submission = calls[2];
  assert.notEqual(submission, undefined);
  assert.equal(
    new Headers(submission?.init.headers).get("idempotency-key"),
    COMMAND_ID,
  );
  assert.deepEqual(
    (submission?.body as { command: unknown }).command,
    {
      type: "pointer_move",
      target: { x: 120, y: 300 },
      duration_ms: 350,
      curve: "smooth",
    },
  );
  assert.equal((submission?.body as { lease_id: unknown }).lease_id, LEASE_ID);
  await lease.renew();
  await lease.release();
  assert.equal(lease.active, false);
  await assert.rejects(() => lease.keyboard.press("Enter"), {
    code: "lease_released",
  });
  assert.equal(calls.length, 5);
});

test("transport failure does not replay a submitted mutation", async () => {
  let calls = 0;
  const mockFetch: typeof fetch = async () => {
    calls += 1;
    if (calls === 1) return jsonResponse(status());
    throw new Error("ambiguous disconnect");
  };
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: mockFetch,
  });
  await assert.rejects(
    () => client.desktop().submit(
      { type: "desktop_probe" },
      { commandId: COMMAND_ID },
    ),
    (error: unknown) => error instanceof XenoteerError
      && error.code === "transport",
  );
  assert.equal(calls, 2);
});

test("window and element query results retain canonical generation counters", async () => {
  const replies = [
    status(),
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      snapshot_revision: "9007199254740993",
      windows: [],
      next_cursor: null,
    },
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      status: "satisfied",
      evaluated_revision: "9007199254740994",
      predicate_satisfied: true,
      matched_count: 1,
      windows: [],
    },
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      atspi_generation: "18446744073709551615",
      snapshot_revision: "9007199254740995",
      order: "preorder",
      elements: [],
      visited_nodes: 0,
      truncated: false,
      warnings: [],
      next_cursor: null,
    },
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      atspi_generation: "18446744073709551615",
      status: "satisfied",
      evaluated_revision: "9007199254740996",
      predicate_satisfied: true,
      matched_count: 1,
      elements: [],
      poll_fallback_used: false,
      truncated: false,
      warnings: [],
    },
  ];
  const mockFetch: typeof fetch = async () => jsonResponse(replies.shift());
  const desktop = (
    await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: mockFetch,
    })
  ).desktop();
  const windows = await desktop.windows.query({
    selector: {
      type: "predicate",
      predicate: { type: "active", value: true },
    },
    order: "creation_ascending",
  });
  const windowWait = await desktop.windows.wait({
    target: {
      type: "selector",
      selector: {
        type: "predicate",
        predicate: { type: "active", value: true },
      },
      quantifier: "any",
    },
    predicate: { type: "exists" },
    timeout_ms: 10_000,
  });
  const elementSelector = {
    scope: { type: "desktop" },
    predicates: [],
    order: "preorder",
  } as const;
  const elements = await desktop.accessibility.query({
    selector: elementSelector,
  });
  const elementWait = await desktop.accessibility.wait({
    target: {
      type: "selector",
      selector: elementSelector,
      quantifier: "any",
    },
    predicate: { type: "exists" },
    timeout_ms: 10_000,
    allow_poll_fallback: false,
  });
  assert.equal(windows.snapshot_revision, "9007199254740993");
  assert.equal(windowWait.evaluated_revision, "9007199254740994");
  assert.equal(elements.atspi_generation, "18446744073709551615");
  assert.equal(elementWait.evaluated_revision, "9007199254740996");
});
