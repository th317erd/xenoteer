// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import test from "node:test";

import {
  decodeEventMessage,
  EventSession,
  XenoteerClient,
  XenoteerError,
  type AuthenticatedWebSocketOptions,
  type JsonObject,
  type WebSocketLike,
} from "../src/index.js";

const TOKEN = "typescript-hardening-token-0123456789abcdef";
const DESKTOP_ID = "20000000-0000-4000-8000-000000000001";
const GENERATION = "30000000-0000-4000-8000-000000000001";
const LEASE_ID = "40000000-0000-4000-8000-000000000001";
const COMMAND_ID = "50000000-0000-4000-8000-000000000001";
const ARTIFACT_ID = "60000000-0000-4000-8000-000000000001";

function status(): Record<string, unknown> {
  return {
    server_version: "0.2.0",
    protocol_min: { major: 1, minor: 0 },
    protocol_max: { major: 1, minor: 0 },
    server_time: "2030-01-01T00:00:00Z",
    desktop: { id: DESKTOP_ID, generation: GENERATION, state: "ready" },
    capabilities: { capabilities: [] },
  };
}

function json(body: unknown, statusCode = 200, type = "application/json"): Response {
  return new Response(JSON.stringify(body), {
    status: statusCode,
    headers: { "content-type": type },
  });
}

function accepted(commandId: string): Record<string, unknown> {
  return {
    command_id: commandId,
    lifecycle: "accepted",
    effect_stage: "accepted",
    accepted_at: "2030-01-01T00:00:00Z",
    warnings: [],
  };
}

type Listener = (event?: unknown) => void;

class FakeSocket implements WebSocketLike {
  readyState = 0;
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
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.emit("close", { code, reason });
  }

  forceClose(code = 1006): void {
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

function welcome(
  generation = GENERATION,
  resumeStatus: "not_requested" | "replayed" | "resync_required" = "not_requested",
): Record<string, unknown> {
  return {
    type: "server.welcome",
    protocol: { major: 1, minor: 0 },
    connection_id: "70000000-0000-4000-8000-000000000001",
    principal: { id: "test", capabilities: [] },
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

function websocketResponder(
  generation = GENERATION,
): (socket: FakeSocket, message: Record<string, unknown>) => void {
  return (socket, message) => {
    if (message["type"] === "client.hello") {
      queueMicrotask(() => socket.message(welcome(
        generation,
        message["resume"] === null ? "not_requested" : "replayed",
      )));
    } else if (message["type"] === "events.subscribe") {
      queueMicrotask(() => socket.message({
        type: "events.subscribed",
        request_id: message["request_id"],
        topics: message["topics"],
      }));
    } else if (message["type"] === "events.unsubscribe") {
      queueMicrotask(() => socket.message({
        type: "events.unsubscribed",
        request_id: message["request_id"],
      }));
    } else if (message["type"] === "client.ping") {
      queueMicrotask(() => socket.message({
        type: "server.pong",
        request_id: message["request_id"],
        nonce: message["nonce"],
      }));
    }
  };
}

function sentSubscriptionRequestId(socket: FakeSocket): string {
  const request = socket.sent
    .map((wire) => JSON.parse(wire) as Record<string, unknown>)
    .filter((message) => message["type"] === "events.subscribe")
    .at(-1);
  assert.equal(typeof request?.["request_id"], "string");
  if (request === undefined) throw new Error("subscription request was not sent");
  return request["request_id"] as string;
}

function eventFrame(
  requestId: string,
  sequence: string,
  overrides: {
    readonly desktopId?: string;
    readonly generation?: string;
    readonly topic?: string;
  } = {},
): Record<string, unknown> {
  return {
    type: "event",
    request_id: requestId,
    event: {
      desktop_id: overrides.desktopId ?? DESKTOP_ID,
      desktop_generation: overrides.generation ?? GENERATION,
      sequence,
      topic: overrides.topic ?? "action.lifecycle",
      payload: {},
    },
  };
}

async function settleSocketDispatch(): Promise<void> {
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function nextEventWithin(
  session: EventSession,
  timeoutMs = 200,
): Promise<IteratorResult<import("../src/index.js").XenoteerEvent>> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      session[Symbol.asyncIterator]().next(),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error("timed out waiting for queued event marker")),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

test("plaintext HTTP is numeric-loopback-only while HTTPS accepts DNS origins", { timeout: 10_000 }, async () => {
  const mock: typeof fetch = async () => json(status());
  await XenoteerClient.connect({ baseUrl: "https://xenoteer.example:9443", token: TOKEN, fetch: mock });
  await XenoteerClient.connect({ baseUrl: "http://127.9.8.7:8080", token: TOKEN, fetch: mock });
  for (const baseUrl of ["http://localhost:8080", "http://xenoteer.example", "ftp://127.0.0.1"]) {
    await assert.rejects(
      () => XenoteerClient.connect({ baseUrl, token: TOKEN, fetch: mock }),
      { code: "invalid_base_url" },
    );
  }
});

test("pre-I/O CommandSubmission reuses exact ID and exact bytes after ambiguity", { timeout: 10_000 }, async () => {
  const bodies: string[] = [];
  let calls = 0;
  const mock: typeof fetch = async (_input, init = {}) => {
    calls += 1;
    if (calls === 1) return json(status());
    assert.equal(typeof init.body, "string");
    bodies.push(init.body as string);
    if (calls === 2) throw new Error(`disconnect ${TOKEN}`);
    return json(accepted(COMMAND_ID));
  };
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: mock,
  });
  const submission = client.desktop().prepareSubmission(
    {
      type: "selection_set",
      selection: "clipboard",
      content: { source: "inline_text", text: "COMMAND_SECRET" },
    },
    { commandId: COMMAND_ID, leaseId: LEASE_ID },
  );
  assert.equal(calls, 1);
  assert.equal(String(submission).includes("COMMAND_SECRET"), false);
  assert.equal(String(submission).includes(LEASE_ID), false);
  await assert.rejects(() => submission.send(), { code: "transport" });
  const handle = await submission.send();
  assert.equal(handle.id, COMMAND_ID);
  assert.equal(bodies.length, 2);
  assert.equal(bodies[0], bodies[1]);
});

test("request timeout, caller abort, oversized response, and problem are bounded and redacted", { timeout: 10_000 }, async () => {
  const hanging: typeof fetch = async (_input, init = {}) => {
    if (String(_input).endsWith("/v1/status")) return json(status());
    return await new Promise<Response>((_resolve, reject) => {
      if (init.signal?.aborted) {
        reject(new Error(`network ${TOKEN}`));
        return;
      }
      init.signal?.addEventListener("abort", () => reject(new Error(`network ${TOKEN}`)), { once: true });
    });
  };
  const timeoutClient = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    requestTimeoutMs: 20,
    fetch: hanging,
  });
  await assert.rejects(
    () => timeoutClient.desktop().windows.list(),
    (error: unknown) => error instanceof XenoteerError
      && error.code === "request_timeout"
      && !String(error).includes(TOKEN)
      && !JSON.stringify(error).includes(TOKEN),
  );

  const controller = new AbortController();
  const abortPromise = timeoutClient.desktop().windows.list({}, {
    signal: controller.signal,
    timeoutMs: 1_000,
  });
  controller.abort("local");
  await assert.rejects(() => abortPromise, { code: "request_cancelled" });

  let calls = 0;
  const hostile: typeof fetch = async () => {
    calls += 1;
    if (calls === 1) return json(status());
    if (calls === 2) {
      return new Response("{}", {
        headers: { "content-type": "application/json", "content-length": "9999999" },
      });
    }
    return json(
      { code: "authentication_failed", detail: `echo ${TOKEN}`, status: 401 },
      401,
      "application/problem+json",
    );
  };
  const hostileClient = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: hostile,
  });
  await assert.rejects(() => hostileClient.desktop().windows.list(), { code: "response_too_large" });
  await assert.rejects(
    () => hostileClient.desktop().windows.list(),
    (error: unknown) => error instanceof XenoteerError
      && error.problemCode === "authentication_failed"
      && !String(error).includes(TOKEN),
  );
});

test("client close fences existing desktop and lease objects", { timeout: 10_000 }, async () => {
  const replies = [
    status(),
    {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      state: "held_by_caller",
      lease_id: LEASE_ID,
      expires_at: "2030-01-01T00:01:00Z",
    },
  ];
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(replies.shift()),
  });
  const desktop = client.desktop();
  const lease = await desktop.acquireControl();
  await client.close();
  await assert.rejects(() => desktop.windows.list(), { code: "client_closed" });
  await assert.rejects(() => lease.renew(), { code: "client_closed" });
});

test("WebSocket waits for welcome, correlates filtered subscribe ack, and never puts token in URL", { timeout: 10_000 }, async () => {
  const sockets: FakeSocket[] = [];
  let socketOptions: AuthenticatedWebSocketOptions | undefined;
  const client = await XenoteerClient.connect({
    baseUrl: "https://xenoteer.example",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession((options) => {
    socketOptions = options;
    const socket = new FakeSocket(websocketResponder());
    sockets.push(socket);
    return socket;
  }, { handshakeTimeoutMs: 1_000, acknowledgmentTimeoutMs: 1_000 });
  assert.equal(socketOptions?.url, "wss://xenoteer.example/v1/ws");
  assert.equal(socketOptions?.url.includes(TOKEN), false);
  assert.equal(socketOptions?.authorization, `Bearer ${TOKEN}`);
  await session.subscribe([
    "command.lifecycle",
    "action.lifecycle",
    "future.window.changed",
  ]);
  const subscriptionRequestId = sentSubscriptionRequestId(sockets[0] as FakeSocket);
  sockets[0]?.message({
    type: "event",
    request_id: subscriptionRequestId,
    event: {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      sequence: "9007199254740993",
      topic: "future.window.changed",
      payload: { future: true },
    },
  });
  const event = await session[Symbol.asyncIterator]().next();
  assert.equal(event.done, false);
  assert.equal(event.value?.kind, "unknown");
  assert.equal(session.lastSequence, 9_007_199_254_740_993n);
  await session.close();
});

test("WebSocket events require the active subscription and exact scope", { timeout: 10_000 }, async () => {
  const cases = [
    {
      name: "no active subscription",
      subscribe: false,
      frame: (activeRequestId: string) => eventFrame(activeRequestId, "1"),
    },
    {
      name: "stale subscription request ID",
      subscribe: true,
      frame: () => eventFrame(
        "80000000-0000-4000-8000-000000000099",
        "1",
      ),
    },
    {
      name: "wrong desktop ID",
      subscribe: true,
      frame: (activeRequestId: string) => eventFrame(activeRequestId, "1", {
        desktopId: "20000000-0000-4000-8000-000000000002",
      }),
    },
    {
      name: "wrong desktop generation",
      subscribe: true,
      frame: (activeRequestId: string) => eventFrame(activeRequestId, "1", {
        generation: "30000000-0000-4000-8000-000000000002",
      }),
    },
    {
      name: "unsubscribed topic",
      subscribe: true,
      frame: (activeRequestId: string) => eventFrame(activeRequestId, "1", {
        topic: "process.exited",
      }),
    },
  ] as const;

  for (const testCase of cases) {
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => json(status()),
    });
    let socket: FakeSocket | undefined;
    const session = await client.openEventSession(
      () => {
        socket = new FakeSocket(websocketResponder());
        return socket;
      },
      { reconnect: { maxAttempts: 0 } },
    );
    try {
      assert.notEqual(socket, undefined);
      let requestId: string = globalThis.crypto.randomUUID();
      if (testCase.subscribe) {
        await session.subscribe(["action.lifecycle"]);
        requestId = sentSubscriptionRequestId(socket as FakeSocket);
      }
      socket?.message(testCase.frame(requestId));
      await settleSocketDispatch();
      assert.equal(
        session.terminalReason?.code,
        "invalid_message",
        testCase.name,
      );
    } finally {
      await session.close();
      await client.close();
    }
  }
});

test("replay and resync markers reject stale subscription request IDs", { timeout: 10_000 }, async () => {
  for (const kind of ["replay", "resync"] as const) {
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => json(status()),
    });
    let socket: FakeSocket | undefined;
    const session = await client.openEventSession(
      () => {
        socket = new FakeSocket(websocketResponder());
        return socket;
      },
      { reconnect: { maxAttempts: 0 } },
    );
    try {
      await session.subscribe(["action.lifecycle"], "1" as import("../src/index.js").CanonicalUInt64);
      socket?.message(kind === "replay"
        ? {
            type: "events.replay_complete",
            request_id: "80000000-0000-4000-8000-000000000098",
            desktop_id: DESKTOP_ID,
            desktop_generation: GENERATION,
            through_sequence: "1",
          }
        : {
            type: "events.resync_required",
            request_id: "80000000-0000-4000-8000-000000000098",
            desktop_id: DESKTOP_ID,
            desktop_generation: GENERATION,
            reason: "history_lost",
            dropped_through: "1",
            latest_sequence: "2",
          });
      await settleSocketDispatch();
      assert.equal(session.terminalReason?.code, "invalid_message", kind);
    } finally {
      await session.close();
      await client.close();
    }
  }
});

test("server generation-change resync closes public desktop and command fences", { timeout: 10_000 }, async () => {
  let httpCalls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      httpCalls += 1;
      return json(status());
    },
  });
  const submission = client.desktop().prepareSubmission(
    { type: "desktop_probe" },
    { commandId: COMMAND_ID },
  );
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(["action.lifecycle"]);
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    socket?.message({
      type: "events.resync_required",
      request_id: requestId,
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      reason: "generation_changed",
      dropped_through: "1",
      latest_sequence: "2",
    });
    await settleSocketDispatch();

    assert.equal(session.terminalReason?.code, "resync_required");
    const marker = await nextEventWithin(session);
    assert.equal(marker.value?.kind, "resync_required");
    assert.equal(
      (marker.value as { readonly reason?: string }).reason,
      "generation_changed",
    );
    assert.throws(() => client.desktop(), {
      code: "stale_reference",
    });
    await assert.rejects(() => submission.send(), {
      code: "generation_changed",
    });
    assert.equal(httpCalls, 1);
  } finally {
    await session.close();
    await client.close();
  }
});

test("wrong-generation events invalidate only the matching desktop lifecycle", { timeout: 10_000 }, async () => {
  const nextGeneration = "30000000-0000-4000-8000-000000000002";
  const otherDesktopId = "20000000-0000-4000-8000-000000000002";
  for (const testCase of [
    {
      name: "matching desktop with wrong generation",
      desktopId: DESKTOP_ID,
      invalidates: true,
    },
    {
      name: "wrong desktop",
      desktopId: otherDesktopId,
      invalidates: false,
    },
  ] as const) {
    let httpCalls = 0;
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => {
        httpCalls += 1;
        return json(status());
      },
    });
    const submission = client.desktop().prepareSubmission(
      { type: "desktop_probe" },
      { commandId: COMMAND_ID },
    );
    let socket: FakeSocket | undefined;
    const session = await client.openEventSession(
      () => {
        socket = new FakeSocket(websocketResponder());
        return socket;
      },
      { reconnect: { maxAttempts: 0 } },
    );
    try {
      await session.subscribe(["action.lifecycle"]);
      const requestId = sentSubscriptionRequestId(socket as FakeSocket);
      socket?.message(eventFrame(requestId, "1", {
        desktopId: testCase.desktopId,
        generation: nextGeneration,
      }));
      await settleSocketDispatch();

      assert.equal(session.terminalReason?.code, "invalid_message", testCase.name);
      if (testCase.invalidates) {
        assert.throws(() => client.desktop(), {
          code: "stale_reference",
        });
        await assert.rejects(() => submission.send(), {
          code: "generation_changed",
        });
      } else {
        const currentDesktop = client.desktop();
        assert.equal(currentDesktop.id, DESKTOP_ID);
        assert.equal(currentDesktop.generation, GENERATION);
        assert.doesNotThrow(() => currentDesktop.prepareSubmission(
          { type: "desktop_probe" },
          { commandId: globalThis.crypto.randomUUID() },
        ));
      }
      assert.equal(httpCalls, 1);
    } finally {
      await session.close();
      await client.close();
    }
  }
});

test("WebSocket ignores an exact duplicate but resynchronizes on true sequence regression", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { capacity: 4, reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(["action.lifecycle"]);
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    socket?.message(eventFrame(requestId, "10"));
    await settleSocketDispatch();
    socket?.message(eventFrame(requestId, "10"));
    await settleSocketDispatch();
    assert.equal(session.lastSequence, 10n);
    assert.equal(session.terminalReason, undefined);

    socket?.message(eventFrame(requestId, "9"));
    await settleSocketDispatch();
    const terminal = session.terminalReason as
      | { readonly code: string }
      | undefined;
    assert.equal(terminal?.code, "resync_required");
    assert.equal(session.lastSequence, 10n);
    const delivered = await nextEventWithin(session);
    const regression = await nextEventWithin(session);
    assert.equal(delivered.value?.kind, "known");
    assert.equal(
      (regression.value as { readonly kind?: string }).kind,
      "resync_required",
    );
    assert.equal(
      (regression.value as { readonly reason?: string }).reason,
      "sequence_regression",
    );
  } finally {
    await session.close();
    await client.close();
  }
});

test("replay completion regression produces a resynchronization boundary", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(
      ["action.lifecycle"],
      "10" as import("../src/index.js").CanonicalUInt64,
    );
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    socket?.message({
      type: "events.replay_complete",
      request_id: requestId,
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      through_sequence: "9",
    });
    await settleSocketDispatch();
    assert.equal(session.terminalReason?.code, "resync_required");
    const marker = await nextEventWithin(session);
    assert.equal(marker.value?.kind, "resync_required");
    assert.equal(
      (marker.value as { readonly reason?: string }).reason,
      "sequence_regression",
    );
    assert.equal(session.lastSequence, 10n);
  } finally {
    await session.close();
    await client.close();
  }
});

test("queue-full server resync retains the authoritative marker and reason", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { capacity: 1, reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(["action.lifecycle"]);
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    socket?.message(eventFrame(requestId, "1"));
    await settleSocketDispatch();
    socket?.message({
      type: "events.resync_required",
      request_id: requestId,
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      reason: "history_lost",
      dropped_through: "1",
      latest_sequence: "2",
    });
    await settleSocketDispatch();

    assert.equal(session.terminalReason?.code, "resync_required");
    const delivered = await nextEventWithin(session);
    const marker = await nextEventWithin(session);
    assert.equal(delivered.value?.kind, "known");
    assert.equal(marker.value?.kind, "resync_required");
    assert.equal(
      (marker.value as { readonly reason?: string }).reason,
      "history_lost",
    );
  } finally {
    await session.close();
    await client.close();
  }
});

test("queue-full replay completion remains observable in stream order", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { capacity: 1, reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(["action.lifecycle"], "0" as import("../src/index.js").CanonicalUInt64);
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    socket?.message(eventFrame(requestId, "1"));
    await settleSocketDispatch();
    socket?.message({
      type: "events.replay_complete",
      request_id: requestId,
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      through_sequence: "1",
    });
    await settleSocketDispatch();

    const delivered = await nextEventWithin(session);
    const replay = await nextEventWithin(session);
    assert.equal(delivered.value?.kind, "known");
    assert.equal(
      (replay.value as { readonly kind?: string }).kind,
      "replay_complete",
    );
    assert.equal(
      (replay.value as { readonly throughSequence?: bigint }).throughSequence,
      1n,
    );
    assert.equal(session.terminalReason, undefined);
  } finally {
    await session.close();
    await client.close();
  }
});

test("zero-event replay preserves its authoritative cursor marker", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { capacity: 1, reconnect: { maxAttempts: 0 } },
  );
  try {
    await session.subscribe(
      ["action.lifecycle"],
      "7" as import("../src/index.js").CanonicalUInt64,
    );
    const requestId = sentSubscriptionRequestId(socket as FakeSocket);
    const replayPromise = nextEventWithin(session);
    socket?.message({
      type: "events.replay_complete",
      request_id: requestId,
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      through_sequence: "7",
    });
    await settleSocketDispatch();
    const replay = await replayPromise;
    assert.equal(
      (replay.value as { readonly kind?: string }).kind,
      "replay_complete",
    );
    assert.equal(
      (replay.value as { readonly throughSequence?: bigint }).throughSequence,
      7n,
    );
    assert.equal(session.lastSequence, 7n);
  } finally {
    await session.close();
    await client.close();
  }
});

test("reserved replay capacity is bounded and a terminal resync supersedes replay backlog", { timeout: 10_000 }, async () => {
  for (const mode of ["replay_overflow", "terminal_resync"] as const) {
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => json(status()),
    });
    let socket: FakeSocket | undefined;
    const session = await client.openEventSession(
      () => {
        socket = new FakeSocket(websocketResponder());
        return socket;
      },
      { capacity: 1, reconnect: { maxAttempts: 0 } },
    );
    try {
      let requestId = "";
      for (let sequence = 0; sequence < 4; sequence += 1) {
        const wireSequence = String(sequence) as import("../src/index.js").CanonicalUInt64;
        await session.subscribe(["action.lifecycle"], wireSequence);
        requestId = sentSubscriptionRequestId(socket as FakeSocket);
        socket?.message({
          type: "events.replay_complete",
          request_id: requestId,
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          through_sequence: wireSequence,
        });
        await settleSocketDispatch();
      }
      if (mode === "replay_overflow") {
        await session.subscribe(
          ["action.lifecycle"],
          "4" as import("../src/index.js").CanonicalUInt64,
        );
        requestId = sentSubscriptionRequestId(socket as FakeSocket);
        socket?.message({
          type: "events.replay_complete",
          request_id: requestId,
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          through_sequence: "4",
        });
      } else {
        socket?.message({
          type: "events.resync_required",
          request_id: requestId,
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          reason: "history_lost",
          dropped_through: "3",
          latest_sequence: "4",
        });
      }
      await settleSocketDispatch();
      assert.equal(
        session.terminalReason?.code,
        mode === "replay_overflow" ? "backpressure" : "resync_required",
      );
      const kinds: string[] = [];
      for (;;) {
        const next = await session[Symbol.asyncIterator]().next();
        if (next.done) break;
        kinds.push(next.value.kind);
      }
      assert.equal(
        kinds.filter((kind) => kind === "replay_complete").length,
        mode === "replay_overflow" ? 4 : 3,
      );
      assert.equal(
        kinds.filter((kind) => kind === "resync_required").length,
        mode === "replay_overflow" ? 0 : 1,
      );
    } finally {
      await session.close();
      await client.close();
    }
  }
});

test("WebSocket malformed, oversize, and slow-consumer losses have explicit terminal reasons", { timeout: 10_000 }, async () => {
  for (const mode of ["malformed", "oversize", "backpressure"] as const) {
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => json(status()),
    });
    let socket: FakeSocket | undefined;
    const session = await client.openEventSession(() => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    }, { capacity: 1, maxMessageBytes: 1024, reconnect: { maxAttempts: 0 } });
    if (mode === "malformed") {
      socket?.emit("message", { data: "{" });
    } else if (mode === "oversize") {
      socket?.emit("message", { data: "x".repeat(1025) });
    } else {
      await session.subscribe(["action.lifecycle"]);
      assert.notEqual(socket, undefined);
      const requestId = sentSubscriptionRequestId(socket as FakeSocket);
      for (const sequence of ["1", "2"]) {
        socket?.message({
          type: "event",
          request_id: requestId,
          event: {
            desktop_id: DESKTOP_ID,
            desktop_generation: GENERATION,
            sequence,
            topic: "action.lifecycle",
            payload: {},
          },
        });
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.equal(
      session.terminalReason?.code,
      mode === "malformed" ? "invalid_message" : mode === "oversize" ? "message_too_large" : "backpressure",
    );
  }
});

test("WebSocket reconnect resumes the exact generation and restores subscription", { timeout: 10_000 }, async () => {
  const sockets: FakeSocket[] = [];
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession(() => {
    const socket = new FakeSocket(websocketResponder());
    sockets.push(socket);
    return socket;
  }, {
    handshakeTimeoutMs: 1_000,
    acknowledgmentTimeoutMs: 1_000,
    reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 2 },
  });
  await session.subscribe(["action.lifecycle"]);
  const subscriptionRequestId = sentSubscriptionRequestId(sockets[0] as FakeSocket);
  sockets[0]?.message({
    type: "event",
    request_id: subscriptionRequestId,
    event: {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      sequence: "40",
      topic: "action.lifecycle",
      payload: {},
    },
  });
  await session[Symbol.asyncIterator]().next();
  sockets[0]?.forceClose();
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(sockets.length, 2);
  const hello = JSON.parse(sockets[1]?.sent[0] ?? "{}") as Record<string, unknown>;
  assert.deepEqual(hello["resume"], {
    desktop_id: DESKTOP_ID,
    desktop_generation: GENERATION,
    event_sequence: "40",
  });
  const subscribe = sockets[1]?.sent
    .map((wire) => JSON.parse(wire) as Record<string, unknown>)
    .find((message) => message["type"] === "events.subscribe");
  assert.equal(subscribe?.["since_sequence"], "40");
  await session.close();
});

test("artifact transfer is bounded, digest-checked, and redacted", { timeout: 10_000 }, async () => {
  const bytes = new TextEncoder().encode("ARTIFACT_SECRET");
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  let call = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (_input, init = {}) => {
      call += 1;
      if (call === 1) return json(status());
      if (call === 2) {
        assert.equal(new Headers(init.headers).get("x-content-sha256"), digest);
        return json({
          artifact_id: ARTIFACT_ID,
          purpose: "clipboard_input",
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          content_type: "application/octet-stream",
          content_length: bytes.byteLength,
          sha256: digest,
          created_at: "2030-01-01T00:00:00Z",
          expires_at: "2030-01-01T00:01:00Z",
        });
      }
      if (call === 3) {
        return new Response(bytes, {
          headers: {
            "content-type": "application/octet-stream",
            "content-length": String(bytes.byteLength),
          },
        });
      }
      return new Response(null, { status: 204 });
    },
  });
  const artifact = await client.desktop().artifacts.uploadClipboardInput(bytes);
  assert.equal(String(artifact).includes(ARTIFACT_ID), false);
  assert.equal(String(artifact).includes("ARTIFACT_SECRET"), false);
  assert.deepEqual(await artifact.download(), bytes);
  await artifact.delete();
});

test("viewer ticket and clipboard diagnostic surfaces redact bearer/content", { timeout: 10_000 }, async () => {
  const viewerSecret = "viewer-ticket-secret-0123456789abcdef";
  let call = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "https://xenoteer.example",
    token: TOKEN,
    fetch: async () => {
      call += 1;
      if (call === 1) return json(status());
      return json({
        ticket: viewerSecret,
        principal_id: "operator",
        audience: "viewer_websocket",
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        origin: "https://viewer.example",
        mode: "view_only",
        issued_at: "2030-01-01T00:00:00Z",
        expires_at: "2030-01-01T00:00:30Z",
        use_policy: "single_use",
      });
    },
  });
  const ticket = await client.desktop().viewer.issueTicket("https://viewer.example");
  assert.equal(ticket.consumeSecret(), viewerSecret);
  assert.equal(String(ticket).includes(viewerSecret), false);
  assert.equal(JSON.stringify(ticket).includes(viewerSecret), false);
  assert.equal(client.desktop().viewer.pageUrl().includes(viewerSecret), false);
});

test("WebSocket handshake has an explicit bounded timeout", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  await assert.rejects(
    () => client.openEventSession(
      () => new FakeSocket(() => undefined),
      { handshakeTimeoutMs: 20, reconnect: { maxAttempts: 0 } },
    ),
    { code: "websocket_timeout" },
  );
  await assert.rejects(
    () => client.openEventSession(
      () => new FakeSocket(() => undefined, false),
      { handshakeTimeoutMs: 20, reconnect: { maxAttempts: 0 } },
    ),
    { code: "websocket_timeout" },
  );
});

test("WebSocket missing pong terminates with heartbeat_timeout", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession(() => {
    return new FakeSocket((socket, message) => {
      if (message["type"] === "client.hello") {
        queueMicrotask(() => socket.message({
          ...welcome(),
          limits: {
            max_message_bytes: 1_048_576,
            heartbeat_ms: 100,
            normal_outbound_capacity: 16,
            reserved_outbound_capacity: 4,
            max_command_watches: 16,
          },
        }));
      }
    });
  }, {
    heartbeatGraceMs: 10,
    reconnect: { maxAttempts: 0 },
  });
  await new Promise((resolve) => setTimeout(resolve, 250));
  assert.equal(session.terminalReason?.code, "heartbeat_timeout");
});

test("WebSocket rejects a correlated pong with the wrong nonce", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession(() => {
    return new FakeSocket((socket, message) => {
      if (message["type"] === "client.hello") {
        queueMicrotask(() => socket.message({
          ...welcome(),
          limits: {
            max_message_bytes: 1_048_576,
            heartbeat_ms: 100,
            normal_outbound_capacity: 16,
            reserved_outbound_capacity: 4,
            max_command_watches: 16,
          },
        }));
      } else if (message["type"] === "client.ping") {
        queueMicrotask(() => socket.message({
          type: "server.pong",
          request_id: message["request_id"],
          nonce: "wrong-nonce",
        }));
      }
    });
  }, {
    heartbeatGraceMs: 10,
    reconnect: { maxAttempts: 0 },
  });
  await new Promise((resolve) => setTimeout(resolve, 180));
  assert.equal(session.terminalReason?.code, "invalid_message");
});

test("event topics use the frozen exact 0..32 contract", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  let socket: FakeSocket | undefined;
  const session = await client.openEventSession(
    () => {
      socket = new FakeSocket(websocketResponder());
      return socket;
    },
    { reconnect: { maxAttempts: 0 } },
  );
  await session.subscribe([]);
  const catchAllRequestId = sentSubscriptionRequestId(socket as FakeSocket);
  socket?.message(eventFrame(catchAllRequestId, "1", {
    topic: "future.authorized_topic",
  }));
  await settleSocketDispatch();
  assert.equal((await nextEventWithin(session)).value?.kind, "unknown");
  await session.subscribe(["process.exited", "feature-name.changed"]);
  await assert.rejects(() => session.subscribe(["window.*"]), {
    code: "invalid_request",
  });
  await assert.rejects(
    () => session.subscribe(
      Array.from({ length: 33 }, (_, index) => `topic.${index}`),
    ),
    { code: "invalid_request" },
  );
  await session.unsubscribe();
  await session.close();
  await client.close();
});

test("incoming event topics enforce the frozen ASCII grammar and UTF-8 byte ceiling", { timeout: 10_000 }, () => {
  const event = (topic: string): Record<string, unknown> => ({
    type: "event",
    request_id: globalThis.crypto.randomUUID(),
    event: {
      desktop_id: DESKTOP_ID,
      desktop_generation: GENERATION,
      sequence: "1",
      topic,
      payload: {},
    },
  });
  assert.equal(decodeEventMessage(event("vendor.future_event")).topic, "vendor.future_event");
  for (const topic of ["Window.*", "window..changed", "é", `a.${"b".repeat(127)}`]) {
    assert.throws(() => decodeEventMessage(event(topic)), {
      code: "invalid_response",
    });
  }
});

test("welcome validates identity, principal, capacities, and requested resume disposition", { timeout: 10_000 }, async () => {
  const invalidWelcomes: Record<string, unknown>[] = [
    { ...welcome(), connection_id: "not-a-uuid" },
    { ...welcome(), principal: { id: "", capabilities: [] } },
    {
      ...welcome(),
      limits: {
        ...(welcome()["limits"] as Record<string, unknown>),
        normal_outbound_capacity: 0,
      },
    },
    welcome(GENERATION, "replayed"),
  ];
  for (const invalidWelcome of invalidWelcomes) {
    const client = await XenoteerClient.connect({
      baseUrl: "http://127.0.0.1:8080",
      token: TOKEN,
      fetch: async () => json(status()),
    });
    await assert.rejects(
      () => client.openEventSession(
        () => new FakeSocket((socket, message) => {
          if (message["type"] === "client.hello") {
            queueMicrotask(() => socket.message(invalidWelcome));
          }
        }),
        { reconnect: { maxAttempts: 0 } },
      ),
      { code: "invalid_response" },
    );
    await client.close();
  }
});

test("WebSocket refreshes bearer per attempt and does not retry permanent policy close", { timeout: 10_000 }, async () => {
  let tokenCalls = 0;
  const sockets: FakeSocket[] = [];
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: async () => {
      tokenCalls += 1;
      return `rotating-token-${String(tokenCalls).padStart(32, "0")}`;
    },
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession((options) => {
    assert.match(options.authorization, /^Bearer rotating-token-/u);
    const socket = new FakeSocket(websocketResponder());
    sockets.push(socket);
    return socket;
  }, {
    reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 2 },
  });
  assert.equal(tokenCalls, 2);
  sockets[0]?.forceClose();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(sockets.length, 2);
  assert.equal(tokenCalls, 3);
  sockets[1]?.forceClose(1008);
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(sockets.length, 2);
  assert.equal(session.terminalReason?.code, "transport");
});

test("correlated permanent WebSocket errors reject the request and terminate the session", { timeout: 10_000 }, async () => {
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => json(status()),
  });
  const session = await client.openEventSession(
    () => new FakeSocket((socket, message) => {
      if (message["type"] === "client.hello") {
        queueMicrotask(() => socket.message(welcome()));
      } else if (message["type"] === "events.subscribe") {
        queueMicrotask(() => socket.message({
          type: "error",
          request_id: message["request_id"],
          code: "authentication_required",
          detail: `hostile ${TOKEN}`,
        }));
      }
    }),
    { reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 2 } },
  );
  await assert.rejects(
    () => session.subscribe([]),
    (error: unknown) => error instanceof XenoteerError
      && error.code === "authentication"
      && !String(error).includes(TOKEN),
  );
  assert.equal(session.terminalReason?.code, "transport");
});

test("generation change fences commands, references, and leases", { timeout: 10_000 }, async () => {
  const nextGeneration = "30000000-0000-4000-8000-000000000002";
  let generation = GENERATION;
  let httpCalls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      httpCalls += 1;
      if (httpCalls === 1) return json(status());
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        state: "held_by_caller",
        lease_id: LEASE_ID,
        expires_at: "2030-01-01T00:01:00Z",
      });
    },
  });
  const desktop = client.desktop();
  const submission = desktop.prepareSubmission(
    { type: "desktop_probe" },
    { commandId: COMMAND_ID },
  );
  const lease = await desktop.acquireControl();
  const sockets: FakeSocket[] = [];
  const session = await client.openEventSession(() => {
    const socket = new FakeSocket((current, message) => {
      if (message["type"] === "client.hello") {
        queueMicrotask(() => current.message(welcome(generation)));
      }
    });
    sockets.push(socket);
    return socket;
  }, {
    reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 2 },
  });
  generation = nextGeneration;
  sockets[0]?.forceClose();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(session.terminalReason?.code, "generation_changed");
  await assert.rejects(() => submission.send(), {
    code: "generation_changed",
  });
  assert.throws(() => desktop.assertCurrentReference(), {
    code: "stale_reference",
  });
  await assert.rejects(() => lease.renew(), {
    code: "lease_released",
  });
  assert.equal(httpCalls, 2);
});

test("scoped lease renews and release failure never masks callback failure", { timeout: 10_000 }, async () => {
  let call = 0;
  let renewals = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (_input, init = {}) => {
      call += 1;
      if (call === 1) return json(status());
      if (init.method === "POST" && String(_input).endsWith("/lease")) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          state: "held_by_caller",
          lease_id: LEASE_ID,
          expires_at: new Date(Date.now() + 40).toISOString(),
        });
      }
      if (String(_input).endsWith("/renew")) {
        renewals += 1;
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          state: "held_by_caller",
          lease_id: LEASE_ID,
          expires_at: new Date(Date.now() + 40).toISOString(),
        });
      }
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        state: "vacant",
        lease_id: null,
        expires_at: null,
      });
    },
  });
  await client.desktop().withControl(20, async () => {
    await new Promise((resolve) => setTimeout(resolve, 15));
  });
  assert.ok(renewals >= 1);

  let failureCall = 0;
  const callbackFailure = new Error("callback sentinel");
  const failingClient = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      failureCall += 1;
      if (failureCall === 1) return json(status());
      if (failureCall === 2) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          state: "held_by_caller",
          lease_id: LEASE_ID,
          expires_at: "2030-01-01T00:01:00Z",
        });
      }
      return json(
        {
          code: "backend_unavailable",
          detail: `hostile ${TOKEN}`,
          status: 503,
          details: {},
        },
        503,
        "application/problem+json",
      );
    },
  });
  await assert.rejects(
    () => failingClient.desktop().withControl(1_000, async () => {
      throw callbackFailure;
    }),
    (error: unknown) => error === callbackFailure,
  );
});

test("problem categories preserve safe details without hostile prose", { timeout: 10_000 }, async () => {
  let calls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      calls += 1;
      if (calls === 1) return json(status());
      return json(
        {
          code: "ambiguous_target",
          detail: `hostile ${TOKEN}`,
          status: 409,
          retry: "never",
          effect_stage: "none",
          cleanup: "failed",
          details: {
            candidates: [{ reference: TOKEN, title: "Editor" }],
            secret: TOKEN,
          },
        },
        409,
        "application/problem+json",
      );
    },
  });
  await assert.rejects(
    () => client.desktop().windows.list(),
    (error: unknown) => {
      return error instanceof XenoteerError
        && error.code === "ambiguous"
        && error.retry === "never"
        && error.effectStage === "none"
        && error.cleanup === "failed"
        && Array.isArray(error.details?.["candidates"])
        && !String(error).includes(TOKEN)
        && !JSON.stringify(error).includes(TOKEN);
    },
  );
});

test("artifact midstream failures become typed redacted SDK errors", { timeout: 10_000 }, async () => {
  const streamSecret = "STREAM_FAILURE_SECRET";
  let calls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      calls += 1;
      if (calls === 1) return json(status());
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(Uint8Array.of(1));
          controller.error(new Error(streamSecret));
        },
      });
      return new Response(body, {
        headers: {
          "content-type": "application/octet-stream",
          "content-length": "2",
        },
      });
    },
  });
  const artifact = client.desktop().artifacts.fromRef({
    artifact_id: ARTIFACT_ID,
    purpose: "clipboard_output",
    desktop_id: DESKTOP_ID,
    desktop_generation: GENERATION,
    content_type: "application/octet-stream",
    content_length: 2,
    sha256: "a".repeat(64),
    created_at: "2030-01-01T00:00:00Z",
    expires_at: "2030-01-01T00:01:00Z",
  });
  await assert.rejects(
    () => artifact.download(),
    (error: unknown) => error instanceof XenoteerError
      && error.code === "transport"
      && !String(error).includes(streamSecret)
      && !JSON.stringify(error).includes(streamSecret),
  );
});

test("welcome resync is explicit and exposes authoritative refresh helper", { timeout: 10_000 }, async () => {
  let calls = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      calls += 1;
      if (calls <= 2) return json(status());
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        snapshot_revision: "1",
        windows: [],
        next_cursor: null,
      });
    },
  });
  const session = await client.openEventSession(
    () => new FakeSocket((socket, message) => {
      if (message["type"] === "client.hello") {
        queueMicrotask(() => socket.message(welcome(GENERATION, "resync_required")));
      }
    }),
    {
      resume: {
        desktopId: DESKTOP_ID,
        desktopGeneration: GENERATION,
        eventSequence: "1" as import("../src/index.js").CanonicalUInt64,
      },
    },
  );
  assert.equal(session.terminalReason?.code, "resync_required");
  const next = await session[Symbol.asyncIterator]().next();
  assert.equal(next.value?.kind, "resync_required");
  const refreshed = await session.refreshAuthoritativeSnapshots();
  assert.equal((refreshed["status"] as { desktop: { generation: string } }).desktop.generation, GENERATION);
  assert.equal(calls, 3);
});
