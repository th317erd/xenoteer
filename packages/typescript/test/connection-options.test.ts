// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import test from "node:test";

import {
  EventSession,
  XenoteerClient,
  XenoteerError,
  type AuthenticatedWebSocketOptions,
  type ArtifactRef,
  type ReconnectPolicy,
  type SafeLogEvent,
  type SafeLogHook,
  type WebSocketFactory,
  type WebSocketLike,
  type XenoteerClientOptions,
} from "../src/index.js";
import { ClientConnectionState, HttpTransport } from "../src/transport.js";

const TOKEN_A = "typescript-connection-token-a-0123456789abcdef";
const TOKEN_B = "typescript-connection-token-b-0123456789abcdef";
const TOKEN_C = "typescript-connection-token-c-0123456789abcdef";
const TOKEN_D = "typescript-connection-token-d-0123456789abcdef";
const TOKEN_E = "typescript-connection-token-e-0123456789abcdef";
const DESKTOP_ID = "21000000-0000-4000-8000-000000000001";
const GENERATION = "31000000-0000-4000-8000-000000000001";
const NEXT_GENERATION = "31000000-0000-4000-8000-000000000002";
const COMMAND_ID = "51000000-0000-4000-8000-000000000001";
const ARTIFACT_ID = "61000000-0000-4000-8000-000000000001";
const ROUTE_CANARY = "query-secret-canary";
const BODY_CANARY = "body-secret-canary";
const PROVIDER_CANARY = "provider-secret-canary";
const SERVER_CANARY = "server-secret-canary";

function status(generation = GENERATION): Record<string, unknown> {
  return {
    server_version: "0.2.0",
    protocol_min: { major: 1, minor: 0 },
    protocol_max: { major: 1, minor: 0 },
    server_time: "2030-01-01T00:00:00Z",
    desktop: { id: DESKTOP_ID, generation, state: "ready" },
    capabilities: { capabilities: [] },
  };
}

function json(body: unknown, statusCode = 200, type = "application/json"): Response {
  return new Response(JSON.stringify(body), {
    status: statusCode,
    headers: { "content-type": type },
  });
}

function welcome(
  generation = GENERATION,
  resumeStatus: "not_requested" | "replayed" = "not_requested",
): Record<string, unknown> {
  return {
    type: "server.welcome",
    protocol: { major: 1, minor: 0 },
    connection_id: "71000000-0000-4000-8000-000000000001",
    principal: { id: "connection-test", capabilities: [] },
    desktop: { id: DESKTOP_ID, generation, state: "ready" },
    limits: {
      max_message_bytes: 1_048_576,
      heartbeat_ms: 60_000,
      normal_outbound_capacity: 16,
      reserved_outbound_capacity: 4,
      max_command_watches: 16,
    },
    resume: { status: resumeStatus },
  };
}

type Listener = (event?: unknown) => void;

class FakeSocket implements WebSocketLike {
  readyState = 0;
  closeCalls = 0;
  readonly sent: string[] = [];
  readonly #listeners = new Map<string, Listener[]>();
  readonly #onSend: (socket: FakeSocket, message: Record<string, unknown>) => void;

  constructor(
    onSend: (socket: FakeSocket, message: Record<string, unknown>) => void,
    autoOpen = true,
  ) {
    this.#onSend = onSend;
    if (autoOpen) {
      queueMicrotask(() => {
        this.readyState = 1;
        this.emit("open");
      });
    }
  }

  send(data: string): void {
    this.sent.push(data);
    this.#onSend(this, JSON.parse(data) as Record<string, unknown>);
  }

  close(code = 1000, reason = ""): void {
    this.closeCalls += 1;
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.emit("close", { code, reason });
  }

  forceClose(code?: number): void {
    this.readyState = 3;
    this.emit("close", { code, reason: "" });
  }

  addEventListener(type: "open", listener: () => void, options?: { once?: boolean }): void;
  addEventListener(type: "message", listener: (event: { readonly data: unknown }) => void): void;
  addEventListener(type: "error", listener: () => void, options?: { once?: boolean }): void;
  addEventListener(
    type: "close",
    listener: (event?: { readonly code?: number; readonly reason?: string }) => void,
    options?: { once?: boolean },
  ): void;
  addEventListener(
    type: "open" | "message" | "error" | "close",
    listener: Function,
    options: { once?: boolean } = {},
  ): void {
    const compatible = listener as Listener;
    const wrapped: Listener = options.once === true
      ? (event) => {
          this.#listeners.set(
            type,
            (this.#listeners.get(type) ?? []).filter((candidate) => candidate !== wrapped),
          );
          compatible(event);
        }
      : compatible;
    this.#listeners.set(type, [...(this.#listeners.get(type) ?? []), wrapped]);
  }

  emit(type: string, event?: unknown): void {
    for (const listener of [...(this.#listeners.get(type) ?? [])]) listener(event);
  }

  message(value: unknown): void {
    this.emit("message", { data: JSON.stringify(value) });
  }
}

function successfulResponder(
  generation = GENERATION,
): (socket: FakeSocket, message: Record<string, unknown>) => void {
  return (socket, message) => {
    if (message["type"] === "client.hello") {
      queueMicrotask(() => socket.message(welcome(
        generation,
        message["resume"] === null ? "not_requested" : "replayed",
      )));
    } else if (message["type"] === "client.ping") {
      queueMicrotask(() => socket.message({
        type: "server.pong",
        request_id: message["request_id"],
        nonce: message["nonce"],
      }));
    }
  };
}

async function waitFor(predicate: () => boolean, timeoutMs = 500): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("condition did not become true");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

type SettledOutcome<T> =
  | { readonly kind: "fulfilled"; readonly value: T }
  | { readonly kind: "rejected"; readonly reason: unknown }
  | { readonly kind: "watchdog" };

async function settleWithin<T>(
  promise: Promise<T>,
  timeoutMs = 150,
): Promise<SettledOutcome<T>> {
  return await new Promise((resolve) => {
    const timer = setTimeout(() => resolve({ kind: "watchdog" }), timeoutMs);
    void promise.then(
      (value) => {
        clearTimeout(timer);
        resolve({ kind: "fulfilled", value });
      },
      (reason: unknown) => {
        clearTimeout(timer);
        resolve({ kind: "rejected", reason });
      },
    );
  });
}

function artifactRef(
  bytes: Uint8Array,
  sha256: string,
): ArtifactRef {
  return {
    artifact_id: ARTIFACT_ID,
    purpose: "clipboard_input",
    desktop_id: DESKTOP_ID,
    desktop_generation: GENERATION,
    content_type: "application/octet-stream",
    content_length: bytes.byteLength,
    sha256,
    created_at: "2030-01-01T00:00:00Z",
    expires_at: "2030-01-01T00:01:00Z",
  };
}

test("public connection-policy types compile as a closed documented contract", { timeout: 10_000 }, () => {
  const reconnect: ReconnectPolicy = {
    maxAttempts: 2,
    initialDelayMs: 1,
    maxDelayMs: 2,
  };
  const hook: SafeLogHook = (event) => {
    const operation: SafeLogEvent["operation"] = event.operation;
    assert.equal(typeof operation, "string");
  };
  const factory: WebSocketFactory = (options: AuthenticatedWebSocketOptions) => {
    assert.equal(typeof options.maxMessageBytes, "number");
    assert.equal(typeof options.handshakeDeadlineMs, "number");
    return new FakeSocket(successfulResponder());
  };
  const options: XenoteerClientOptions = {
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    webSocketFactory: factory,
    reconnect,
    webSocketQueueCapacity: 64,
    webSocketMaxMessageBytes: 2048,
    webSocketHandshakeTimeoutMs: 250,
    webSocketAcknowledgmentTimeoutMs: 250,
    webSocketHeartbeatGraceMs: 250,
    webSocketCloseTimeoutMs: 250,
    log: hook,
  };
  assert.equal(options.webSocketFactory, factory);
});

test("root policy is retained, mutation-isolated, and refreshes A/B/C/D credentials", { timeout: 10_000 }, async () => {
  const tokens = [TOKEN_A, TOKEN_B, TOKEN_C, TOKEN_D, TOKEN_E];
  let tokenCalls = 0;
  let fetchCalls = 0;
  const reconnect = { maxAttempts: 1, initialDelayMs: 1, maxDelayMs: 1 };
  const sockets: FakeSocket[] = [];
  const factoryInputs: AuthenticatedWebSocketOptions[] = [];
  const factory: WebSocketFactory = (factoryOptions) => {
    factoryInputs.push(factoryOptions);
    const socket = new FakeSocket(successfulResponder());
    sockets.push(socket);
    return socket;
  };
  const clientOptions = {
    baseUrl: "http://127.0.0.1:8080",
    token: async () => {
      const token = tokens[tokenCalls];
      tokenCalls += 1;
      if (token === undefined) throw new Error("unexpected token call");
      return token;
    },
    fetch: async () => {
      fetchCalls += 1;
      return json(status());
    },
    webSocketFactory: factory,
    reconnect,
    webSocketQueueCapacity: 32,
    webSocketMaxMessageBytes: 4096,
    webSocketHandshakeTimeoutMs: 200,
    webSocketAcknowledgmentTimeoutMs: 200,
    webSocketHeartbeatGraceMs: 200,
    webSocketCloseTimeoutMs: 200,
    clientName: "retained-client",
    clientVersion: "9.8.7",
  };
  const client = await XenoteerClient.connect(clientOptions);
  await client.refreshStatus();
  reconnect.maxAttempts = 0;
  reconnect.initialDelayMs = 60_000;
  reconnect.maxDelayMs = 60_000;
  clientOptions.webSocketMaxMessageBytes = 16 * 1_048_576;
  clientOptions.webSocketHandshakeTimeoutMs = 60_000;
  clientOptions.clientName = "mutated-client";

  const before = Date.now();
  const session = await client.openEventSession();
  const after = Date.now();
  assert.equal(fetchCalls, 2);
  assert.equal(tokenCalls, 3);
  assert.equal(factoryInputs[0]?.authorization, `Bearer ${TOKEN_C}`);
  assert.equal(factoryInputs[0]?.maxMessageBytes, 4096);
  assert.ok((factoryInputs[0]?.handshakeDeadlineMs ?? 0) >= before + 150);
  assert.ok((factoryInputs[0]?.handshakeDeadlineMs ?? Infinity) <= after + 250);
  const firstHello = JSON.parse(sockets[0]?.sent[0] ?? "{}") as Record<string, unknown>;
  assert.deepEqual(firstHello["client"], { name: "retained-client", version: "9.8.7" });
  assert.deepEqual(firstHello["protocol"], {
    major: 1,
    min_minor: 0,
    max_minor: 0,
  });

  sockets[0]?.forceClose();
  await waitFor(() => sockets.length === 2);
  assert.equal(tokenCalls, 4);
  assert.equal(factoryInputs[1]?.authorization, `Bearer ${TOKEN_D}`);
  assert.equal(factoryInputs[1]?.maxMessageBytes, 4096);
  await session.close();
  await client.close();
});

test("events require an explicit factory and the compatibility overload remains valid", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    fetch: async () => json(status()),
  });
  await assert.rejects(() => client.openEventSession(), {
    code: "invalid_request",
  });
  const session = await client.openEventSession(
    () => new FakeSocket(successfulResponder()),
    { reconnect: { maxAttempts: 0 } },
  );
  await session.close();
  await client.close();
});

test("root WebSocket and reconnect bounds reject before network I/O", { timeout: 10_000 }, async () => {
  const invalidPolicies: Array<Partial<XenoteerClientOptions>> = [
    { webSocketQueueCapacity: 0 },
    { webSocketMaxMessageBytes: 1023 },
    { webSocketHandshakeTimeoutMs: 0 },
    { webSocketAcknowledgmentTimeoutMs: 60_001 },
    { webSocketHeartbeatGraceMs: 120_001 },
    { webSocketCloseTimeoutMs: 30_001 },
    { reconnect: { maxAttempts: 101 } },
    { reconnect: { initialDelayMs: 2, maxDelayMs: 1 } },
  ];
  for (const invalid of invalidPolicies) {
    let fetchCalls = 0;
    await assert.rejects(
      () => XenoteerClient.connect({
        baseUrl: "http://127.0.0.1:8080",
        token: TOKEN_A,
        fetch: async () => {
          fetchCalls += 1;
          return json(status());
        },
        ...invalid,
      }),
      { code: "invalid_request" },
    );
    assert.equal(fetchCalls, 0);
  }
});

test("protocol range and client descriptors reject before any HTTP or WebSocket I/O", { timeout: 10_000 }, async () => {
  const invalidPolicies: Array<Partial<XenoteerClientOptions>> = [
    { protocolRange: { major: 2, minMinor: 0, maxMinor: 0 } },
    { protocolRange: { major: 1, minMinor: 1, maxMinor: 0 } },
    { protocolRange: { major: 1, minMinor: -1, maxMinor: 0 } },
    {
      protocolRange: null as unknown as NonNullable<
        XenoteerClientOptions["protocolRange"]
      >,
    },
    { clientName: "" },
    { clientName: "controller\nname" },
    { clientName: "é".repeat(65) },
    { clientVersion: "" },
    { clientVersion: "1\u0000.0" },
    { clientVersion: "🙂".repeat(33) },
    { clientName: 7 as unknown as string },
  ];
  for (const invalid of invalidPolicies) {
    let fetchCalls = 0;
    let factoryCalls = 0;
    await assert.rejects(
      () => XenoteerClient.connect({
        baseUrl: "http://127.0.0.1:8080",
        token: TOKEN_A,
        fetch: async () => {
          fetchCalls += 1;
          return json(status());
        },
        webSocketFactory: () => {
          factoryCalls += 1;
          return new FakeSocket(successfulResponder());
        },
        ...invalid,
      }),
      { code: "invalid_request" },
    );
    assert.equal(fetchCalls, 0, JSON.stringify(invalid));
    assert.equal(factoryCalls, 0, JSON.stringify(invalid));
  }
});

test("the initial hello is checked against the configured outbound frame bound", { timeout: 10_000 }, async () => {
  const logs: SafeLogEvent[] = [];
  const sockets: FakeSocket[] = [];
  const state = new ClientConnectionState();
  const hello = {
    type: "client.hello",
    request_id: "81000000-0000-4000-8000-000000000001",
    protocol: { major: 1, min_minor: 0, max_minor: 0 },
    client: { name: "x".repeat(512), version: "1" },
    resume: null,
  };
  const outcome = await settleWithin(EventSession.connect(
    () => {
      const socket = new FakeSocket(successfulResponder());
      sockets.push(socket);
      return socket;
    },
    { url: "ws://127.0.0.1:8080/v1/ws", authorization: `Bearer ${TOKEN_A}` },
    hello,
    state,
    {
      maxMessageBytes: 128,
      handshakeTimeoutMs: 50,
      closeTimeoutMs: 10,
      reconnect: { maxAttempts: 0 },
      safeLog: (event) => logs.push(event),
    },
  ));
  if (outcome.kind === "fulfilled") await outcome.value.close();
  assert.equal(outcome.kind, "rejected");
  assert.equal(
    outcome.kind === "rejected" && outcome.reason instanceof XenoteerError
      ? outcome.reason.code
      : undefined,
    "request_too_large",
  );
  assert.equal(sockets.length, 0);
  assert.deepEqual(
    logs.map((event) => [event.outcome, event.errorCode]),
    [
      ["started", undefined],
      ["failed", "request_too_large"],
    ],
  );
  await state.close();
});

test("never-settling HTTP token providers obey request deadlines for JSON and streams", { timeout: 10_000 }, async () => {
  for (const operation of ["json", "stream"] as const) {
    const logs: SafeLogEvent[] = [];
    let fetchCalls = 0;
    const transport = new HttpTransport({
      baseUrl: "http://127.0.0.1:8080",
      token: async () => await new Promise<string>(() => undefined),
      requestTimeoutMs: 15,
      log: (event) => logs.push(event),
      fetch: async () => {
        fetchCalls += 1;
        return operation === "json"
          ? json(status())
          : new Response(Uint8Array.of(1), {
              headers: { "content-type": "application/octet-stream" },
            });
      },
    });
    const pending = operation === "json"
      ? transport.request("GET", "/v1/status")
      : transport.downloadStream(`/v1/artifacts/${ARTIFACT_ID}/content`);
    const outcome = await settleWithin(pending);
    assert.equal(outcome.kind, "rejected", operation);
    assert.equal(
      outcome.kind === "rejected" && outcome.reason instanceof XenoteerError
        ? outcome.reason.code
        : undefined,
      "request_timeout",
      operation,
    );
    assert.equal(fetchCalls, 0, operation);
    assert.deepEqual(
      logs.map((event) => [event.operation, event.outcome, event.errorCode]),
      [
        [operation === "json" ? "http.request" : "artifact.download", "started", undefined],
        [operation === "json" ? "http.request" : "artifact.download", "failed", "request_timeout"],
      ],
      operation,
    );
    await transport.close();
  }
});

test("never-settling initial and reconnect WebSocket token providers obey one handshake deadline", { timeout: 10_000 }, async () => {
  for (const phase of ["initial", "reconnect"] as const) {
    const logs: SafeLogEvent[] = [];
    const sockets: FakeSocket[] = [];
    let tokenCalls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: async () => {
        tokenCalls += 1;
        if (tokenCalls === 1 || (phase === "reconnect" && tokenCalls === 2)) return TOKEN_A;
        return await new Promise<string>(() => undefined);
      },
      fetch: async () => json(status()),
      log: (event) => logs.push(event),
      webSocketHandshakeTimeoutMs: 15,
      webSocketCloseTimeoutMs: 10,
      reconnect: { maxAttempts: 1, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const socket = new FakeSocket(successfulResponder());
        sockets.push(socket);
        return socket;
      },
    });
    if (phase === "initial") {
      const outcome = await settleWithin(client.openEventSession());
      assert.equal(outcome.kind, "rejected");
      assert.equal(
        outcome.kind === "rejected" && outcome.reason instanceof XenoteerError
          ? outcome.reason.code
          : undefined,
        "websocket_timeout",
      );
      assert.equal(sockets.length, 0);
    } else {
      const session = await client.openEventSession();
      sockets[0]?.forceClose();
      await waitFor(() => session.terminalReason !== undefined, 150);
      assert.equal(session.terminalReason?.code, "transport");
      assert.equal(sockets.length, 1);
    }
    const handshakes = logs.filter((event) => event.operation === "websocket.handshake");
    assert.deepEqual(
      handshakes.map((event) => [event.attempt, event.outcome, event.errorCode]),
      phase === "initial"
        ? [
            [1, "started", undefined],
            [1, "failed", "websocket_timeout"],
          ]
        : [
            [1, "started", undefined],
            [1, "succeeded", undefined],
            [2, "started", undefined],
            [2, "failed", "websocket_timeout"],
          ],
    );
    await client.close();
  }
});

test("safe logs use frozen closed route templates for JSON and every artifact operation", { timeout: 10_000 }, async () => {
  const events: SafeLogEvent[] = [];
  const bytes = new TextEncoder().encode(BODY_CANARY);
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  let call = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    log: (event) => {
      assert.equal(Object.isFrozen(event), true);
      events.push(event);
    },
    fetch: async (_input, init = {}) => {
      call += 1;
      if (call <= 2) return json(status());
      if (String(_input).includes("/windows?")) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          snapshot_revision: "1",
          windows: [],
          next_cursor: null,
        });
      }
      if (init.method === "POST") return json(artifactRef(bytes, digest), 201);
      if (init.method === "GET") {
        return new Response(bytes, {
          headers: {
            "content-type": "application/octet-stream",
            "content-length": String(bytes.byteLength),
            "x-content-sha256": digest,
          },
        });
      }
      return new Response(null, { status: 204 });
    },
  });
  await client.refreshStatus();
  await client.desktop().windows.list({ cursor: ROUTE_CANARY });
  const artifact = await client.desktop().artifacts.uploadClipboardInput(bytes);
  assert.deepEqual(await artifact.download(), bytes);
  await artifact.delete();

  const projections = events.map((event) => ({
    operation: event.operation,
    outcome: event.outcome,
    route: event.route,
  }));
  assert.deepEqual(projections, [
    { operation: "http.request", outcome: "started", route: "/v1/status" },
    { operation: "http.request", outcome: "succeeded", route: "/v1/status" },
    { operation: "http.request", outcome: "started", route: "/v1/status" },
    { operation: "http.request", outcome: "succeeded", route: "/v1/status" },
    { operation: "http.request", outcome: "started", route: "/v1/desktops/:desktop_id/windows" },
    { operation: "http.request", outcome: "succeeded", route: "/v1/desktops/:desktop_id/windows" },
    { operation: "artifact.upload", outcome: "started", route: "/v1/artifacts" },
    { operation: "artifact.upload", outcome: "succeeded", route: "/v1/artifacts" },
    { operation: "artifact.download", outcome: "started", route: "/v1/artifacts/:artifact_id" },
    { operation: "artifact.download", outcome: "succeeded", route: "/v1/artifacts/:artifact_id" },
    { operation: "artifact.delete", outcome: "started", route: "/v1/artifacts/:artifact_id" },
    { operation: "artifact.delete", outcome: "succeeded", route: "/v1/artifacts/:artifact_id" },
  ]);
  for (const event of events) {
    assert.deepEqual(
      Object.keys(event).sort(),
      Object.keys(event).filter((key) => [
        "attempt",
        "errorCode",
        "method",
        "operation",
        "outcome",
        "requestBytes",
        "responseBytes",
        "route",
        "status",
      ].includes(key)).sort(),
    );
  }
  const serialized = JSON.stringify(events);
  for (const canary of [
    TOKEN_A,
    BODY_CANARY,
    ROUTE_CANARY,
    ARTIFACT_ID,
    DESKTOP_ID,
    GENERATION,
  ]) {
    assert.equal(serialized.includes(canary), false);
  }
  await client.close();
});

test("route classification fails closed and JSON attempts pair token, transport, status, and parse failures", { timeout: 10_000 }, async () => {
  const cases = [
    {
      name: "token",
      token: async (): Promise<string> => {
        throw new Error(PROVIDER_CANARY);
      },
      fetch: async (): Promise<Response> => {
        throw new Error("fetch must not run");
      },
      errorCode: "invalid_token",
    },
    {
      name: "transport",
      token: TOKEN_A,
      fetch: async (): Promise<Response> => {
        throw new Error(PROVIDER_CANARY);
      },
      errorCode: "transport",
    },
    {
      name: "status",
      token: TOKEN_A,
      fetch: async (): Promise<Response> => json(
        { code: "service_unavailable", detail: SERVER_CANARY },
        503,
        "application/problem+json",
      ),
      errorCode: "backend",
    },
    {
      name: "parse",
      token: TOKEN_A,
      fetch: async (): Promise<Response> => new Response(`{${SERVER_CANARY}`, {
        headers: { "content-type": "application/json" },
      }),
      errorCode: "invalid_response",
    },
  ] as const;
  for (const item of cases) {
    const events: SafeLogEvent[] = [];
    await assert.rejects(() => XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: item.token,
      fetch: item.fetch,
      log: (event) => events.push(event),
    }));
    assert.equal(events.length, 2, item.name);
    assert.deepEqual(events.map((event) => event.outcome), ["started", "failed"]);
    assert.equal(events[0]?.route, "/v1/status");
    assert.equal(events[1]?.errorCode, item.errorCode);
    const serialized = JSON.stringify(events);
    assert.equal(serialized.includes(PROVIDER_CANARY), false);
    assert.equal(serialized.includes(SERVER_CANARY), false);
  }
});

test("unknown paths collapse to the literal unknown route without raw path or query", { timeout: 10_000 }, async () => {
  const events: SafeLogEvent[] = [];
  const transport = new HttpTransport({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    log: (event) => events.push(event),
    fetch: async () => json({ ok: true }),
  });
  assert.deepEqual(
    await transport.request("GET", `/v1/${ROUTE_CANARY}?token=${TOKEN_A}`),
    { ok: true },
  );
  assert.deepEqual(events.map((event) => event.route), ["unknown", "unknown"]);
  const serialized = JSON.stringify(events);
  assert.equal(serialized.includes(ROUTE_CANARY), false);
  assert.equal(serialized.includes(TOKEN_A), false);
  await transport.close();
});

test("stream download logs success only at EOF and failure on early return or stream error", { timeout: 10_000 }, async () => {
  const bytes = Uint8Array.of(1, 2);
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  for (const mode of ["complete", "early-return", "error", "digest-mismatch"] as const) {
    const events: SafeLogEvent[] = [];
    let calls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      log: (event) => events.push(event),
      fetch: async () => {
        calls += 1;
        if (calls === 1) return json(status());
        const body = new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(Uint8Array.of(1));
            if (mode === "error") {
              controller.error(new Error(SERVER_CANARY));
            } else {
              controller.enqueue(Uint8Array.of(2));
              controller.close();
            }
          },
        });
        return new Response(body, {
          headers: {
            "content-type": "application/octet-stream",
            "content-length": "2",
            "x-content-sha256": mode === "digest-mismatch" ? "a".repeat(64) : digest,
          },
        });
      },
    });
    const artifact = client.desktop().artifacts.fromRef(artifactRef(
      bytes,
      mode === "digest-mismatch" ? "a".repeat(64) : digest,
    ));
    const before = events.length;
    if (mode === "complete") {
      assert.deepEqual(await artifact.download(), bytes);
    } else if (mode === "early-return") {
      const iterator = artifact.stream();
      assert.deepEqual((await iterator.next()).value, Uint8Array.of(1));
      assert.deepEqual(events.slice(before).map((event) => event.outcome), ["started"]);
      await iterator.return(undefined);
    } else {
      await assert.rejects(() => artifact.download(), {
        code: mode === "error" ? "transport" : "invalid_response",
      });
    }
    const attempt = events.slice(before);
    assert.deepEqual(
      attempt.map((event) => event.outcome),
      mode === "complete" ? ["started", "succeeded"] : ["started", "failed"],
    );
    assert.equal(JSON.stringify(attempt).includes(SERVER_CANARY), false);
    await client.close();
  }
});

test("artifact upload, download metadata/status, and delete parse failures each emit one failed pair", { timeout: 10_000 }, async () => {
  const bytes = Uint8Array.of(1, 2);
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  for (const mode of [
    "upload",
    "upload-metadata",
    "download",
    "content-type",
    "delete",
  ] as const) {
    const events: SafeLogEvent[] = [];
    let responseCancelled = false;
    let calls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      log: (event) => events.push(event),
      fetch: async () => {
        calls += 1;
        if (calls === 1) return json(status());
        if (mode === "upload") {
          return new Response(`{${SERVER_CANARY}`, {
            status: 201,
            headers: { "content-type": "application/json" },
          });
        }
        if (mode === "upload-metadata") {
          return json(artifactRef(bytes, "b".repeat(64)), 201);
        }
        if (mode === "download") {
          return json(
            { code: "backend_unavailable", detail: SERVER_CANARY },
            503,
            "application/problem+json",
          );
        }
        if (mode === "content-type") {
          return new Response(new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(bytes);
            },
            cancel() {
              responseCancelled = true;
            },
          }), {
            headers: {
              "content-type": "text/plain",
              "content-length": String(bytes.byteLength),
              "x-content-sha256": digest,
            },
          });
        }
        return new Response(SERVER_CANARY, { status: 200 });
      },
    });
    const artifact = client.desktop().artifacts.fromRef(artifactRef(bytes, digest));
    if (mode === "upload" || mode === "upload-metadata") {
      await assert.rejects(
        () => client.desktop().artifacts.uploadClipboardInput(bytes),
        { code: "invalid_response" },
      );
    } else if (mode === "download") {
      await assert.rejects(() => artifact.download(), { code: "backend" });
    } else if (mode === "content-type") {
      await assert.rejects(() => artifact.download(), { code: "invalid_response" });
      assert.equal(responseCancelled, true);
    } else {
      await assert.rejects(() => artifact.delete(), { code: "invalid_response" });
    }
    const operation = mode === "upload" || mode === "upload-metadata"
      ? "artifact.upload"
      : mode === "download" || mode === "content-type"
        ? "artifact.download"
        : "artifact.delete";
    const attempt = events.filter((event) => event.operation === operation);
    assert.deepEqual(attempt.map((event) => event.outcome), ["started", "failed"]);
    assert.equal(JSON.stringify(attempt).includes(SERVER_CANARY), false);
    await client.close();
  }
});

test("stream uploads pair source success and source failure without logging callback errors", { timeout: 10_000 }, async () => {
  const bytes = Uint8Array.of(1, 2);
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  for (const mode of [
    "complete",
    "source-error",
    "short-source",
    "wrong-digest",
  ] as const) {
    const events: SafeLogEvent[] = [];
    let calls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      log: (event) => events.push(event),
      fetch: async (_input, init = {}) => {
        calls += 1;
        if (calls === 1) return json(status());
        assert.equal(init.body instanceof ReadableStream, true);
        const reader = (init.body as ReadableStream<Uint8Array>).getReader();
        while (!(await reader.read()).done) {
          // Deliberately consume the injected source like a real fetch adapter.
        }
        return json(artifactRef(
          bytes,
          mode === "wrong-digest" ? "a".repeat(64) : digest,
        ), 201);
      },
    });
    const source = (async function* (): AsyncGenerator<Uint8Array> {
      yield Uint8Array.of(1);
      if (mode === "source-error") throw new Error(PROVIDER_CANARY);
      if (mode === "short-source") return;
      yield Uint8Array.of(2);
    })();
    const operation = client.desktop().artifacts.uploadClipboardInputStream(
      source,
      {
        contentLength: 2,
        sha256: mode === "wrong-digest" ? "a".repeat(64) : digest,
      },
    );
    if (mode === "complete") {
      const artifact = await operation;
      assert.equal(artifact.ref.artifact_id, ARTIFACT_ID);
    } else {
      await assert.rejects(
        () => operation,
        { code: mode === "source-error" ? "transport" : "invalid_response" },
      );
    }
    const attempt = events.filter((event) => event.operation === "artifact.upload");
    assert.deepEqual(
      attempt.map((event) => event.outcome),
      mode === "complete" ? ["started", "succeeded"] : ["started", "failed"],
    );
    assert.equal(JSON.stringify(attempt).includes(PROVIDER_CANARY), false);
    await client.close();
  }
});

test("throwing hooks cannot alter or replay an HTTP mutation", { timeout: 10_000 }, async () => {
  let postCalls = 0;
  const hook: SafeLogHook = () => {
    throw new Error("logging must be observational");
  };
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    log: hook,
    fetch: async (_input, init = {}) => {
      if (init.method !== "POST") return json(status());
      postCalls += 1;
      return json({
        command_id: COMMAND_ID,
        lifecycle: "accepted",
        effect_stage: "accepted",
        accepted_at: "2030-01-01T00:00:00Z",
        warnings: [],
      }, 202);
    },
  });
  const handle = await client.desktop().prepareSubmission(
    { type: "desktop_probe" },
    { commandId: COMMAND_ID },
  ).send();
  assert.equal(handle.id, COMMAND_ID);
  assert.equal(postCalls, 1);
  await client.close();
});

test("WebSocket token, factory, transport, timeout, and welcome failures all pair safely", { timeout: 10_000 }, async () => {
  for (const mode of ["token", "factory", "transport", "timeout", "welcome"] as const) {
    const events: SafeLogEvent[] = [];
    const sockets: FakeSocket[] = [];
    let tokenCalls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: async () => {
        tokenCalls += 1;
        if (mode === "token" && tokenCalls === 2) {
          throw new Error(PROVIDER_CANARY);
        }
        return TOKEN_A;
      },
      fetch: async () => json(status()),
      log: (event) => events.push(event),
      webSocketHandshakeTimeoutMs: 10,
      webSocketCloseTimeoutMs: 10,
      webSocketFactory: () => {
        if (mode === "factory") throw new Error(PROVIDER_CANARY);
        const socket = mode === "transport"
          ? new FakeSocket(() => undefined, false)
          : new FakeSocket((current, message) => {
              if (message["type"] !== "client.hello") return;
              if (mode === "welcome") {
                queueMicrotask(() => current.message({
                  ...welcome(),
                  principal: null,
                  detail: SERVER_CANARY,
                }));
              }
            });
        sockets.push(socket);
        if (mode === "transport") queueMicrotask(() => socket.emit("error"));
        return socket;
      },
    });
    await assert.rejects(() => client.openEventSession());
    const attempts = events.filter((event) => event.operation === "websocket.handshake");
    assert.deepEqual(attempts.map((event) => event.outcome), ["started", "failed"]);
    assert.equal(attempts[1]?.errorCode, {
      token: "invalid_token",
      factory: "transport",
      transport: "transport",
      timeout: "websocket_timeout",
      welcome: "invalid_response",
    }[mode]);
    assert.equal(JSON.stringify(attempts).includes(PROVIDER_CANARY), false);
    assert.equal(JSON.stringify(attempts).includes(SERVER_CANARY), false);
    if (sockets[0] !== undefined) assert.equal(sockets[0].closeCalls, 1);
    await client.close();
  }
});

test("pre-welcome policy close codes are permanent initially and during reconnect", { timeout: 10_000 }, async () => {
  for (const closeCode of [1008, 4401, 4403]) {
    const initialSockets: FakeSocket[] = [];
    const initialLogs: SafeLogEvent[] = [];
    const initialClient = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      log: (event) => initialLogs.push(event),
      webSocketFactory: () => {
        const socket = new FakeSocket((current, message) => {
          if (message["type"] === "client.hello") {
            queueMicrotask(() => current.forceClose(closeCode));
          }
        });
        initialSockets.push(socket);
        return socket;
      },
    });
    await assert.rejects(
      () => initialClient.openEventSession(),
      {
        code: closeCode === 4401 ? "authentication" : "permission",
      },
    );
    assert.equal(initialSockets.length, 1);
    assert.equal(initialSockets[0]?.closeCalls, 1);
    const handshakes = initialLogs.filter(
      (event) => event.operation === "websocket.handshake",
    );
    assert.deepEqual(
      handshakes.map((event) => event.outcome),
      ["started", "failed"],
    );
    assert.equal(
      handshakes[1]?.errorCode,
      closeCode === 4401 ? "authentication" : "permission",
    );
    await initialClient.close();

    const reconnectSockets: FakeSocket[] = [];
    const reconnectClient = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 3, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = reconnectSockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] !== "client.hello") return;
          queueMicrotask(() => {
            if (index === 0) current.message(welcome());
            else current.forceClose(closeCode);
          });
        });
        reconnectSockets.push(socket);
        return socket;
      },
    });
    const session = await reconnectClient.openEventSession();
    reconnectSockets[0]?.forceClose();
    await waitFor(() => session.terminalReason !== undefined);
    await new Promise((resolve) => setTimeout(resolve, 5));
    assert.equal(reconnectSockets.length, 2);
    assert.equal(reconnectSockets[1]?.closeCalls, 1);
    assert.equal(session.terminalReason?.code, "transport");
    await reconnectClient.close();
  }
});

test("reconnect subscription restoration has bounded permanent and transient failure paths", { timeout: 10_000 }, async () => {
  for (const mode of ["permanent", "transient"] as const) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] === "client.hello") {
            queueMicrotask(() => current.message(welcome(
              GENERATION,
              message["resume"] === null ? "not_requested" : "replayed",
            )));
          } else if (message["type"] === "events.subscribe") {
            queueMicrotask(() => {
              if (index === 1) {
                current.message({
                  type: "error",
                  request_id: message["request_id"],
                  status: mode === "permanent" ? 401 : 503,
                  code: mode === "permanent"
                    ? "authentication_required"
                    : "server_busy",
                  detail: SERVER_CANARY,
                });
              } else {
                current.message({
                  type: "events.subscribed",
                  request_id: message["request_id"],
                  topics: message["topics"],
                });
              }
            });
          }
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    await session.subscribe(["action.lifecycle"]);
    sockets[0]?.forceClose();
    if (mode === "permanent") {
      await waitFor(() => session.terminalReason !== undefined);
      await new Promise((resolve) => setTimeout(resolve, 5));
      assert.equal(sockets.length, 2);
      assert.equal(sockets[1]?.closeCalls, 1);
      assert.equal(session.terminalReason?.code, "transport");
    } else {
      await waitFor(() => sockets.length === 3);
      await waitFor(() => (sockets[2]?.sent ?? []).some((wire) => {
        const message = JSON.parse(wire) as Record<string, unknown>;
        return message["type"] === "events.subscribe";
      }));
      assert.equal(sockets[1]?.closeCalls, 1);
      assert.equal(session.terminalReason, undefined);
      await session.close();
    }
    await client.close();
  }
});

test("WebSocket attempts have paired safe logs and every failed candidate is closed once", { timeout: 10_000 }, async () => {
  const events: SafeLogEvent[] = [];
  const sockets: FakeSocket[] = [];
  let factoryCalls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    fetch: async () => json(status()),
    reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 1 },
    webSocketHandshakeTimeoutMs: 100,
    webSocketCloseTimeoutMs: 100,
    log: (event) => events.push(event),
    webSocketFactory: (options) => {
      factoryCalls += 1;
      assert.equal(options.maxMessageBytes, 1_048_576);
      const socket = new FakeSocket((current, message) => {
        if (message["type"] !== "client.hello") return;
        if (factoryCalls === 2) {
          queueMicrotask(() => current.message({
            type: "error",
            status: 503,
            code: "server_busy",
            detail: SERVER_CANARY,
          }));
        } else {
          queueMicrotask(() => current.message(welcome(
            GENERATION,
            message["resume"] === null ? "not_requested" : "replayed",
          )));
        }
      });
      sockets.push(socket);
      return socket;
    },
  });
  const session = await client.openEventSession();
  sockets[0]?.forceClose();
  await waitFor(() => sockets.length === 3);
  assert.equal(sockets[1]?.closeCalls, 1);
  const handshakes = events.filter((event) => event.operation === "websocket.handshake");
  assert.deepEqual(
    handshakes.map((event) => [event.attempt, event.outcome]),
    [
      [1, "started"],
      [1, "succeeded"],
      [2, "started"],
      [2, "failed"],
      [3, "started"],
      [3, "succeeded"],
    ],
  );
  assert.equal(handshakes[3]?.errorCode, "unexpected_http_status");
  assert.equal(JSON.stringify(handshakes).includes(SERVER_CANARY), false);
  assert.equal(JSON.stringify(handshakes).includes(TOKEN_A), false);
  await session.close();
  await client.close();
});

test("401 and 403 welcome failures stop on the first reconnect candidate", { timeout: 10_000 }, async () => {
  for (const statusCode of [401, 403]) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 3, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] !== "client.hello") return;
          queueMicrotask(() => current.message(index === 0
            ? welcome()
            : {
                type: "error",
                status: statusCode,
                code: statusCode === 401 ? "authentication_required" : "permission_denied",
                detail: SERVER_CANARY,
              }));
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose();
    await waitFor(() => session.terminalReason !== undefined);
    await new Promise((resolve) => setTimeout(resolve, 5));
    assert.equal(sockets.length, 2, String(statusCode));
    assert.equal(sockets[1]?.closeCalls, 1, String(statusCode));
    assert.equal(session.terminalReason?.code, "transport");
    await client.close();
  }
});

test("protocol and generation handshake failures are permanent and do not consume retries", { timeout: 10_000 }, async () => {
  for (const mode of [
    "protocol-welcome",
    "generation-welcome",
    "protocol-error",
    "generation-error",
  ] as const) {
    const sockets: FakeSocket[] = [];
    const logs: SafeLogEvent[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 3, initialDelayMs: 1, maxDelayMs: 1 },
      log: (event) => logs.push(event),
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] !== "client.hello") return;
          const response = index === 0
            ? welcome()
            : mode === "generation-welcome"
              ? welcome(NEXT_GENERATION)
              : mode === "protocol-welcome"
                ? {
                  ...welcome(),
                  protocol: { major: 1, minor: 1 },
                }
                : {
                    type: "error",
                    status: 409,
                    code: mode === "generation-error"
                      ? "generation_mismatch"
                      : "unsupported_protocol",
                    detail: SERVER_CANARY,
                };
          queueMicrotask(() => current.message(response));
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose();
    await waitFor(() => session.terminalReason !== undefined);
    await new Promise((resolve) => setTimeout(resolve, 5));
    assert.equal(sockets.length, 2, `${mode}: ${JSON.stringify(logs)}`);
    assert.equal(sockets[1]?.closeCalls, 1, mode);
    assert.equal(
      session.terminalReason?.code,
      mode.startsWith("generation") ? "generation_changed" : "transport",
    );
    await client.close();
  }
});

test("closing during reconnect token resolution cancels the provider and pairs the attempt log", { timeout: 10_000 }, async () => {
  const logs: SafeLogEvent[] = [];
  const sockets: FakeSocket[] = [];
  let tokenCalls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: async () => {
      tokenCalls += 1;
      if (tokenCalls <= 2) return TOKEN_A;
      return await new Promise<string>(() => undefined);
    },
    fetch: async () => json(status()),
    log: (event) => logs.push(event),
    webSocketHandshakeTimeoutMs: 60_000,
    webSocketCloseTimeoutMs: 10,
    reconnect: { maxAttempts: 1, initialDelayMs: 1, maxDelayMs: 1 },
    webSocketFactory: () => {
      const socket = new FakeSocket(successfulResponder());
      sockets.push(socket);
      return socket;
    },
  });
  const session = await client.openEventSession();
  sockets[0]?.forceClose();
  await waitFor(() => logs.some(
    (event) => event.operation === "websocket.handshake"
      && event.attempt === 2
      && event.outcome === "started",
  ));
  const started = Date.now();
  await session.close();
  assert.ok(Date.now() - started < 150);
  await waitFor(() => logs.some(
    (event) => event.operation === "websocket.handshake"
      && event.attempt === 2
      && event.outcome === "failed",
  ), 150);
  assert.equal(sockets.length, 1);
  assert.deepEqual(
    logs.filter((event) => event.operation === "websocket.handshake")
      .map((event) => [event.attempt, event.outcome]),
    [
      [1, "started"],
      [1, "succeeded"],
      [2, "started"],
      [2, "failed"],
    ],
  );
  await client.close();
});

test("transport loss closes each established failed socket once before replacement", { timeout: 10_000 }, async () => {
  for (const mode of ["error", "heartbeat-send"] as const) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      webSocketHeartbeatGraceMs: 1,
      webSocketCloseTimeoutMs: 10,
      reconnect: { maxAttempts: 1, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] === "client.hello") {
            const response = welcome();
            const limits = response["limits"] as Record<string, unknown>;
            queueMicrotask(() => current.message({
              ...response,
              limits: { ...limits, heartbeat_ms: 250 },
            }));
          } else if (message["type"] === "client.ping" && mode === "heartbeat-send" && index === 0) {
            throw new Error("simulated send loss");
          } else if (message["type"] === "client.ping") {
            queueMicrotask(() => current.message({
              type: "server.pong",
              request_id: message["request_id"],
              nonce: message["nonce"],
            }));
          }
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    if (mode === "error") {
      sockets[0]?.emit("error");
      sockets[0]?.forceClose();
    }
    await waitFor(() => sockets.length === 2, 750);
    assert.equal(sockets[0]?.closeCalls, 1, mode);
    assert.equal(session.terminalReason, undefined, mode);
    await session.close();
    await client.close();
  }
});

test("closing during reconnect backoff cancels the delay and prevents replacement", { timeout: 10_000 }, async () => {
  const sockets: FakeSocket[] = [];
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN_A,
    fetch: async () => json(status()),
    webSocketCloseTimeoutMs: 10,
    reconnect: { maxAttempts: 1, initialDelayMs: 200, maxDelayMs: 200 },
    webSocketFactory: () => {
      const socket = new FakeSocket(successfulResponder());
      sockets.push(socket);
      return socket;
    },
  });
  const session = await client.openEventSession();
  sockets[0]?.forceClose();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const started = Date.now();
  await session.close();
  assert.ok(Date.now() - started < 100);
  await new Promise((resolve) => setTimeout(resolve, 250));
  assert.equal(sockets.length, 1);
  await client.close();
});

test("only explicit transient close codes reconnect before and after welcome", { timeout: 10_000 }, async () => {
  for (const code of [undefined, 1001, 1012, 1013] as const) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] !== "client.hello") return;
          queueMicrotask(() => {
            if (index === 1) current.forceClose(code);
            else current.message(welcome());
          });
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose();
    await waitFor(() => sockets.length === 3);
    assert.equal(session.terminalReason, undefined);
    await session.close();
    await client.close();
  }

  for (const code of [1000, 1002, 1003, 1007, 1009]) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const index = sockets.length;
        const socket = new FakeSocket((current, message) => {
          if (message["type"] !== "client.hello") return;
          queueMicrotask(() => {
            if (index === 0) current.message(welcome());
            else current.forceClose(code);
          });
        });
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose();
    await waitFor(() => session.terminalReason !== undefined);
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.equal(sockets.length, 2, String(code));
    assert.equal(session.terminalReason?.code, "transport");
    await client.close();
  }

  for (const code of [undefined, 1001, 1012, 1013] as const) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 1, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const socket = new FakeSocket(successfulResponder());
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose(code);
    await waitFor(() => sockets.length === 2);
    assert.equal(session.terminalReason, undefined);
    await session.close();
    await client.close();
  }

  for (const code of [1000, 1002, 1003, 1007, 1009]) {
    const sockets: FakeSocket[] = [];
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      fetch: async () => json(status()),
      reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 1 },
      webSocketFactory: () => {
        const socket = new FakeSocket(successfulResponder());
        sockets.push(socket);
        return socket;
      },
    });
    const session = await client.openEventSession();
    sockets[0]?.forceClose(code);
    await waitFor(() => session.terminalReason !== undefined);
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.equal(sockets.length, 1, String(code));
    assert.equal(session.terminalReason?.code, "transport");
    assert.match(session.terminalReason?.detail ?? "", new RegExp(String(code), "u"));
    await client.close();
  }
});

test("caller headers cannot override SDK authority, negotiation, or framing headers", { timeout: 10_000 }, async () => {
  const stream = (): ReadableStream<Uint8Array> => new ReadableStream({
    start(controller) {
      controller.enqueue(Uint8Array.of(1));
      controller.close();
    },
  });
  const cases: ReadonlyArray<readonly [
    string,
    string,
    (transport: HttpTransport, headers: Readonly<Record<string, string>>) => Promise<unknown>,
  ]> = [
    ["json", "aUtHoRiZaTiOn", (transport, headers) => transport.request("GET", "/v1/status", undefined, { headers })],
    ["upload", "CONTENT-TYPE", (transport, headers) => transport.upload("/v1/artifacts", Uint8Array.of(1), "application/octet-stream", { headers })],
    ["stream-upload", "content-length", (transport, headers) => transport.uploadStream("/v1/artifacts", stream(), 1, "application/octet-stream", { headers })],
    ["download", "Accept", (transport, headers) => transport.download(`/v1/artifacts/${ARTIFACT_ID}`, { headers })],
    ["stream-download", "X-Content-Sha256", (transport, headers) => transport.downloadStream(`/v1/artifacts/${ARTIFACT_ID}`, { headers })],
    ["delete", "authorization", (transport, headers) => transport.deleteEmpty(`/v1/artifacts/${ARTIFACT_ID}`, { headers })],
  ];
  for (const [name, header, invoke] of cases) {
    let fetchCalls = 0;
    let tokenCalls = 0;
    const transport = new HttpTransport({
      baseUrl: "http://127.0.0.1:8080",
      token: async () => {
        tokenCalls += 1;
        return TOKEN_A;
      },
      fetch: async () => {
        fetchCalls += 1;
        throw new Error("reserved header reached fetch");
      },
    });
    await assert.rejects(
      () => invoke(transport, { [header]: TOKEN_B }),
      { code: "invalid_request" },
      name,
    );
    assert.equal(fetchCalls, 0, name);
    assert.equal(tokenCalls, 0, name);
    await transport.close();
  }
});

test("artifact streams validate unique exact length and digest headers before yielding bytes", { timeout: 10_000 }, async () => {
  const body = Uint8Array.of(1, 2);
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", body))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  for (const mode of [
    "length-missing",
    "length-duplicate",
    "length-malformed",
    "length-mismatch",
    "digest-missing",
    "digest-duplicate",
    "digest-malformed",
    "digest-mismatch",
  ] as const) {
    const logs: SafeLogEvent[] = [];
    let fetchCalls = 0;
    let cancelled = false;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN_A,
      log: (event) => logs.push(event),
      fetch: async (_input, init = {}) => {
        fetchCalls += 1;
        if (fetchCalls === 1) return json(status());
        if (init.method === "POST") return json(artifactRef(body, digest), 201);
        const headers = new Headers({
          "content-type": "application/octet-stream",
          "content-length": String(body.byteLength),
          "x-content-sha256": digest,
        });
        if (mode === "length-missing") headers.delete("content-length");
        if (mode === "length-duplicate") headers.append("content-length", String(body.byteLength));
        if (mode === "length-malformed") headers.set("content-length", "+2");
        if (mode === "length-mismatch") headers.set("content-length", "1");
        if (mode === "digest-missing") headers.delete("x-content-sha256");
        if (mode === "digest-duplicate") headers.append("x-content-sha256", digest);
        if (mode === "digest-malformed") headers.set("x-content-sha256", "ABC");
        if (mode === "digest-mismatch") headers.set("x-content-sha256", "0".repeat(64));
        return new Response(new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(body);
            controller.close();
          },
          cancel() {
            cancelled = true;
          },
        }), { headers });
      },
    });
    const artifact = await client.desktop().artifacts.uploadClipboardInput(body);
    const iterator = artifact.stream();
    const outcome = await settleWithin(iterator.next());
    if (outcome.kind === "fulfilled") await iterator.return(undefined);
    assert.equal(outcome.kind, "rejected", mode);
    assert.equal(
      outcome.kind === "rejected" && outcome.reason instanceof XenoteerError
        ? outcome.reason.code
        : undefined,
      "invalid_response",
      mode,
    );
    assert.equal(cancelled, true, mode);
    assert.deepEqual(
      logs.filter((event) => event.operation === "artifact.download")
        .map((event) => [event.outcome, event.errorCode]),
      [
        ["started", undefined],
        ["failed", "invalid_response"],
      ],
      mode,
    );
    await client.close();
  }
});
