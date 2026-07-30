// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import {
  appendFileSync,
  cpSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { promisify } from "node:util";
import test from "node:test";

import {
  XenoteerClient,
  XenoteerError,
} from "../src/index.js";

const execute = promisify(execFile);
const TOKEN = "conformance-package-token-0123456789abcdef";
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

function json(body: unknown, code = 200, type = "application/json"): Response {
  return new Response(JSON.stringify(body), {
    status: code,
    headers: { "content-type": type },
  });
}

function staleProblem(): Response {
  return json(
    {
      code: "stale_reference",
      detail: "reference is stale",
      status: 409,
    },
    409,
    "application/problem+json",
  );
}

function windowEntry(xid: number, birth: string, token: string): Record<string, unknown> {
  return {
    reference_token: token,
    snapshot: {
      ref: {
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        xid,
        observed_generation: birth,
        identity_hash: "a".repeat(64),
      },
      xid_hex: `0x${xid.toString(16)}`,
      model_revision: birth,
      metadata: {},
      process: {},
      state: {},
      has_accessibility_application: false,
      warnings: [],
    },
  };
}

function elementEntry(cacheSequence: string, objectPath: string): Record<string, unknown> {
  return {
    snapshot: {
      ref: {
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        atspi_generation: "1",
        application: {
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          atspi_generation: "1",
          unique_bus_name: ":1.42",
          root_object_path: "/org/a11y/atspi/accessible/root",
          app_instance_generation: "1",
          identity_hash: "b".repeat(64),
        },
        object_path: objectPath,
        object_identity_hash: "c".repeat(64),
        cache_sequence: cacheSequence,
      },
      role: {},
      states: [],
      interfaces: [],
      actions: [],
      attributes: [],
      relations: [],
      window_correlation: {},
      revision: cacheSequence,
      completeness: "complete",
      truncated: false,
      warnings: [],
    },
  };
}

function corpusCase(file: string, id: string): Record<string, unknown> {
  const suite = JSON.parse(
    readFileSync(join(process.cwd(), "../../conformance/v1/cases", file), "utf8"),
  ) as { cases: Array<Record<string, unknown>> };
  const selected = suite.cases.find((candidate) => candidate["id"] === id);
  assert.notEqual(selected, undefined, `missing corpus case ${id}`);
  return structuredClone(selected) as Record<string, unknown>;
}

async function runAdapterCase(
  testCase: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const manifest = JSON.parse(
    readFileSync(join(process.cwd(), "../../conformance/v1/manifest.json"), "utf8"),
  ) as { corpus: string; corpus_sha256: string; protocol: Record<string, unknown> };
  return await runAdapterPayload({
    adapter_protocol: 1,
    cases: [testCase],
    corpus: manifest.corpus,
    corpus_sha256: manifest.corpus_sha256,
    protocol: manifest.protocol,
  });
}

async function runAdapterPayload(
  payload: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const input = JSON.stringify(payload);
  return await new Promise<Record<string, unknown>>((resolve, reject) => {
    const child = spawn(process.execPath, ["scripts/conformance-adapter.mjs"], {
      cwd: process.cwd(),
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error("conformance adapter mutation timed out"));
    }, 3_000);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`conformance adapter exited ${String(code)}: ${stderr}`));
        return;
      }
      try {
        resolve(JSON.parse(stdout) as Record<string, unknown>);
      } catch (error) {
        reject(error);
      }
    });
    child.stdin.end(input);
  });
}

test("official runner executes all 73 TypeScript cases with zero skips", { timeout: 10_000 }, async () => {
  const result = await execute(
    "python3",
    [
      "../../scripts/conformance/run.py",
      "--adapter",
      "node",
      "scripts/conformance-adapter.mjs",
    ],
    { cwd: process.cwd(), timeout: 8_000 },
  );
  assert.match(result.stdout, /adapter passed 73 Xenoteer v1 conformance cases/u);
  assert.equal(result.stderr, "");
});

test("concrete adapter rejects malformed fixtures and incorrect SDK expectations", {
  timeout: 10_000,
}, async () => {
  const mutations: Record<string, unknown>[] = [];

  const missingCommand = corpusCase(
    "command-reconnect.json",
    "command.reconnect.after-acceptance",
  );
  delete (missingCommand["input"] as Record<string, unknown>)["command"];
  mutations.push(missingCommand);

  const malformedResult = corpusCase(
    "effect-stages.json",
    "effect.success.observed",
  );
  delete (
    (malformedResult["input"] as Record<string, unknown>)["result"] as Record<string, unknown>
  )["warnings"];
  mutations.push(malformedResult);

  const malformedFrame = corpusCase(
    "event-continuity.json",
    "event.filtered-sequence-jump",
  );
  const frames = (malformedFrame["input"] as Record<string, unknown>)["frames"] as
    Array<Record<string, unknown>>;
  (frames[0]!["event"] as Record<string, unknown>)["topic"] = "invalid topic";
  mutations.push(malformedFrame);

  const wrongSecret = corpusCase("redaction.json", "redaction.viewer-ticket");
  (
    ((wrongSecret["input"] as Record<string, unknown>)["raw"] as Record<string, unknown>)
      ["ticket"] as Record<string, unknown>
  )["ticket"] = "different-ticket-value";
  mutations.push(wrongSecret);

  const wrongBehavior = corpusCase(
    "effect-stages.json",
    "effect.cancel.after-effect",
  );
  (wrongBehavior["expect"] as Record<string, unknown>)["has_visible_effect"] = false;
  mutations.push(wrongBehavior);

  const reversedSequence = corpusCase(
    "event-continuity.json",
    "event.filtered-sequence-jump",
  );
  (reversedSequence["expect"] as Record<string, unknown>)["delivered_sequences"] = [
    "15",
    "10",
  ];
  mutations.push(reversedSequence);

  for (const mutation of mutations) {
    const output = await runAdapterCase(mutation);
    const results = output["results"] as Array<Record<string, unknown>>;
    assert.equal(results.length, 1);
    assert.equal(results[0]!["status"], "failed", String(mutation["id"]));
  }
});

test("direct adapter observes public negotiation and bearer failure surfaces", {
  timeout: 10_000,
}, async () => {
  const negotiation = corpusCase(
    "protocol-negotiation.json",
    "negotiation.disjoint-minors",
  );
  (negotiation["expect"] as Record<string, unknown>)["sdk_error_observed"] = true;
  const negotiationOutput = await runAdapterCase(negotiation);
  assert.equal(
    (negotiationOutput["results"] as Array<Record<string, unknown>>)[0]?.["status"],
    "passed",
  );

  const bearer = corpusCase("redaction.json", "redaction.bearer-token");
  const bearerExpectation = bearer["expect"] as Record<string, unknown>;
  bearerExpectation["failure_surface_exercised"] = true;
  bearerExpectation["url_surface_observed"] = true;
  const bearerOutput = await runAdapterCase(bearer);
  assert.equal(
    (bearerOutput["results"] as Array<Record<string, unknown>>)[0]?.["status"],
    "passed",
  );
});

test("direct adapter rejects any non-frozen corpus or protocol identity", {
  timeout: 10_000,
}, async () => {
  const manifest = JSON.parse(
    readFileSync(join(process.cwd(), "../../conformance/v1/manifest.json"), "utf8"),
  ) as { corpus: string; corpus_sha256: string; protocol: Record<string, unknown> };
  const testCase = corpusCase(
    "protocol-negotiation.json",
    "negotiation.exact-v1",
  );
  const base = {
    adapter_protocol: 1,
    cases: [testCase],
    corpus: manifest.corpus,
    corpus_sha256: manifest.corpus_sha256,
    protocol: manifest.protocol,
  };
  for (const mutation of [
    { corpus: "xenoteer-conformance-v2" },
    { corpus_sha256: "0".repeat(64) },
    { protocol: { major: 2, min_minor: 0, max_minor: 0 } },
  ]) {
    await assert.rejects(
      () => runAdapterPayload({ ...base, ...mutation }),
      /frozen conformance identity/u,
    );
  }
});

test("deterministic package verifier accepts exactly the Apache SDK boundary", { timeout: 10_000 }, async () => {
  const result = await execute(
    process.execPath,
    ["scripts/verify-package.mjs"],
    { cwd: process.cwd(), timeout: 8_000 },
  );
  assert.match(result.stdout, /verified deterministic Apache package: 61 files/u);
  assert.equal(result.stderr, "");
});

test("package verifier rejects BSL content, hidden tests, and symlink ancestors", { timeout: 10_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "xenoteer-ts-boundary-"));
  const makeFixture = (name: string): string => {
    const destination = join(root, name);
    cpSync(process.cwd(), destination, {
      recursive: true,
      filter: (source) => !source.includes("/node_modules")
        && !source.endsWith(".tgz"),
    });
    return destination;
  };
  const rejected = async (fixture: string, pattern: RegExp): Promise<void> => {
    await assert.rejects(
      () => execute(process.execPath, ["scripts/verify-package.mjs"], {
        cwd: fixture,
        timeout: 3_000,
      }),
      (error: unknown) => {
        const stderr = typeof error === "object"
          && error !== null
          && "stderr" in error
          && typeof error.stderr === "string"
          ? error.stderr
          : "";
        return pattern.test(stderr);
      },
    );
  };
  try {
    const bsl = makeFixture("bsl");
    appendFileSync(
      join(bsl, "dist/src/client.js"),
      "\n/* Business Source License 1.1 */\n",
    );
    await rejected(bsl, /BSL-licensed implementation content/u);

    const hidden = makeFixture("hidden");
    const manifestPath = join(hidden, "package.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as {
      files: string[];
    };
    manifest.files.push("dist/test");
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    await rejected(hidden, /package inventory differs/u);

    const symlinked = makeFixture("symlinked");
    renameSync(join(symlinked, "dist"), join(symlinked, "outside-dist"));
    symlinkSync("outside-dist", join(symlinked, "dist"), "dir");
    await rejected(symlinked, /symlink|package inventory differs/u);
  } finally {
    rmSync(root, { force: true, recursive: true });
  }
});

test("window handle remains stale and explicit relocate returns a new birth", { timeout: 10_000 }, async () => {
  const selector = { type: "predicate", predicate: { type: "active", value: true } } as const;
  const oldEntry = windowEntry(101, "1", "old-window-token-0123456789");
  const newEntry = windowEntry(101, "2", "new-window-token-0123456789");
  let call = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (_input, init = {}) => {
      call += 1;
      if (call === 1) return json(status());
      if (call === 2) {
        const body = JSON.parse(String(init.body)) as Record<string, unknown>;
        assert.equal(body["match_policy"], "exactly_one");
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          snapshot_revision: "1",
          window: oldEntry,
        });
      }
      if (call === 3) return staleProblem();
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        snapshot_revision: "2",
        window: newEntry,
      });
    },
  });
  const handle = await client.desktop().windows.one(selector, "creation_ascending");
  await assert.rejects(() => handle.snapshot(), (error: unknown) => {
    return error instanceof XenoteerError && error.problemCode === "stale_reference";
  });
  assert.equal(handle.stale, true);
  await assert.rejects(() => handle.snapshot(), { code: "stale_reference" });
  const relocated = await handle.relocate();
  assert.notEqual(relocated, handle);
  assert.equal(relocated.identity["observed_generation"], "2");
  assert.equal(handle.identity["observed_generation"], "1");
  assert.equal(handle.stale, true);
});

test("element handle remains stale across bus identity reuse and relocates explicitly", { timeout: 10_000 }, async () => {
  const selector = {
    scope: { type: "desktop" },
    predicates: [],
    order: "preorder",
  } as const;
  const oldEntry = elementEntry("1", "/org/example/Button");
  const newEntry = elementEntry("2", "/org/example/Button");
  let call = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async () => {
      call += 1;
      if (call === 1) return json(status());
      if (call === 2) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          atspi_generation: "1",
          snapshot_revision: "1",
          element: oldEntry,
        });
      }
      if (call === 3) return staleProblem();
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        atspi_generation: "1",
        snapshot_revision: "2",
        element: newEntry,
      });
    },
  });
  const handle = await client.desktop().accessibility.one(selector);
  await assert.rejects(() => handle.snapshot(), {
    problemCode: "stale_reference",
  });
  assert.equal(handle.stale, true);
  const relocated = await handle.relocate();
  assert.equal(relocated.identity["cache_sequence"], "2");
  assert.equal(handle.identity["cache_sequence"], "1");
  assert.equal(handle.stale, true);
});

test("element first requires explicit order and deliberately selects index zero", { timeout: 10_000 }, async () => {
  const entry = elementEntry("1", "/org/example/First");
  let sentBody: Record<string, unknown> | undefined;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (_input, init = {}) => {
      if (init.body === undefined) return json(status());
      sentBody = JSON.parse(String(init.body)) as Record<string, unknown>;
      return json({
        desktop_id: DESKTOP_ID,
        desktop_generation: GENERATION,
        atspi_generation: "1",
        snapshot_revision: "1",
        order: "preorder",
        elements: [entry],
        visited_nodes: 1,
        truncated: false,
        warnings: [],
        next_cursor: null,
      });
    },
  });
  await assert.rejects(
    () => client.desktop().accessibility.first({ scope: { type: "desktop" }, predicates: [] }),
    { code: "invalid_request" },
  );
  const handle = await client.desktop().accessibility.first({
    scope: { type: "desktop" },
    predicates: [],
    order: "preorder",
  });
  const requestSelector = sentBody?.["selector"] as Record<string, unknown>;
  assert.equal(requestSelector["result_index"], 0);
  assert.equal(handle.stale, false);
});

test("window and element handles expose every generation-bound target operation", { timeout: 10_000 }, async () => {
  const windowSelector = {
    type: "predicate",
    predicate: { type: "active", value: true },
  } as const;
  const elementSelector = {
    scope: { type: "desktop" },
    predicates: [],
    order: "preorder",
  } as const;
  const commands: Record<string, unknown>[] = [];
  let windowResolveCount = 0;
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (input, init = {}) => {
      const url = String(input);
      if (url.endsWith("/v1/status")) return json(status());
      if (url.endsWith("/windows/resolve")) {
        windowResolveCount += 1;
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          snapshot_revision: String(windowResolveCount),
          window: windowEntry(
            100 + windowResolveCount,
            String(windowResolveCount),
            `window-token-${String(windowResolveCount).padStart(16, "0")}`,
          ),
        });
      }
      if (url.endsWith("/accessibility/elements/resolve")) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          atspi_generation: "1",
          snapshot_revision: "1",
          element: elementEntry("1", "/org/example/Editable"),
        });
      }
      if (url.endsWith("/commands")) {
        const envelope = JSON.parse(String(init.body)) as Record<string, unknown>;
        commands.push(envelope["command"] as Record<string, unknown>);
        return json({
          command_id: envelope["command_id"],
          lifecycle: "accepted",
          effect_stage: "accepted",
          accepted_at: "2030-01-01T00:00:00Z",
          warnings: [],
        });
      }
      throw new Error(`unexpected handle request: ${url}`);
    },
  });
  const desktop = client.desktop();
  const target = await desktop.windows.one(windowSelector, "creation_ascending");
  const sibling = await desktop.windows.one(windowSelector, "creation_ascending");
  await target.windowStack("above", sibling);
  const element = await desktop.accessibility.one(elementSelector);
  await element.insertText(3, "hello", {
    selection: "collapse_after",
    verifyLengthOnly: true,
  });
  assert.deepEqual(commands[0], {
    type: "window_stack",
    window: target.identity,
    mode: "above",
    sibling: sibling.identity,
  });
  assert.deepEqual(commands[1], {
    type: "element_insert_text",
    element: element.identity,
    offset: 3,
    text: "hello",
    selection: "collapse_after",
    verify_length_only: true,
    postcondition: null,
  });
});

test("terminal artifact cleanup failure preserves command success and records safe expiry fallback", { timeout: 10_000 }, async () => {
  const text = "x".repeat(256 * 1024 + 1);
  let call = 0;
  let digest = "";
  const client = await XenoteerClient.connect({
    baseUrl: "http://127.0.0.1:8080",
    token: TOKEN,
    fetch: async (_input, init = {}) => {
      call += 1;
      if (call === 1) return json(status());
      if (call === 2) {
        return json({
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          state: "held_by_caller",
          lease_id: LEASE_ID,
          expires_at: "2030-01-01T00:01:00Z",
        });
      }
      if (call === 3) {
        digest = new Headers(init.headers).get("x-content-sha256") ?? "";
        return json({
          artifact_id: ARTIFACT_ID,
          purpose: "clipboard_input",
          desktop_id: DESKTOP_ID,
          desktop_generation: GENERATION,
          content_type: "text/plain; charset=utf-8",
          content_length: new TextEncoder().encode(text).byteLength,
          sha256: digest,
          created_at: "2030-01-01T00:00:00Z",
          expires_at: "2030-01-01T00:05:00Z",
        });
      }
      if (call === 4) {
        return json({
          command_id: COMMAND_ID,
          lifecycle: "accepted",
          effect_stage: "accepted",
          accepted_at: "2030-01-01T00:00:00Z",
          warnings: [],
        });
      }
      if (call === 5) {
        return json({
          command_id: COMMAND_ID,
          lifecycle: "succeeded",
          effect_stage: "clipboard_ownership_changed",
          accepted_at: "2030-01-01T00:00:00Z",
          started_at: "2030-01-01T00:00:00.010Z",
          finished_at: "2030-01-01T00:00:00.020Z",
          outcome: { type: "acknowledged" },
          error: null,
          warnings: [],
        });
      }
      return json(
        { code: "artifact_store_unavailable", detail: "delete failed", status: 503 },
        503,
        "application/problem+json",
      );
    },
  });
  const lease = await client.desktop().acquireControl();
  const handle = await lease.clipboard.setText(text, { commandId: COMMAND_ID });
  const result = await handle.waitOnce(10);
  assert.equal(result.lifecycle, "succeeded");
  assert.equal(handle.cleanupError?.message.includes("expiry remains authoritative"), true);
  assert.equal(String(handle.cleanupError).includes(text), false);
  assert.equal(call, 6);
});
