# `@xenoteer/sdk`

Async, ESM-first TypeScript SDK for the frozen Xenoteer v1 desktop automation
API. It requires Node.js 22 or newer and has no runtime dependencies.

The SDK is Apache-2.0 licensed as a separate client package. It does not contain
or depend on the Business Source License server implementation.

## Connect and control

```ts
import { XenoteerClient } from "@xenoteer/sdk";

await using client = await XenoteerClient.connect({
  baseUrl: "https://127.0.0.1:9443",
  token: async () => process.env.XENOTEER_TOKEN!,
});

const desktop = client.desktop(); // immutable ID + generation fence

await desktop.withControl(30_000, async (control) => {
  const move = await control.mouse.move(120, 300, {
    durationMs: 350, // smooth interpolation; omission selects server policy
  });
  await move.waitUntilTerminal(10_000);

  const press = await control.keyboard.press("Enter");
  await press.waitUntilTerminal(10_000);
});
```

`connect()` authenticates `GET /v1/status`, validates the current desktop, and
negotiates the highest shared protocol minor. It never acquires control.
Negotiation rejects expose the stable `reversed_minor_range`,
`unsupported_major`, `no_shared_minor`, or `unsupported_version` SDK error code
that caused the fence to fail.
`Desktop`, `ControlLease`, and `CommandHandle` objects remain bound to the
desktop generation from that status snapshot; they never silently relocate
after restart.

## Safe command recovery

Every mutation is one HTTP attempt. The SDK does not retry `fetch`, invent a
replacement command ID, or replay a command after an ambiguous disconnect.
Create the exact submission before I/O when a workflow needs recovery:

```ts
const submission = desktop.prepareSubmission(
  { type: "desktop_probe" },
  { commandId: crypto.randomUUID() },
);
const handle = await submission.send();
```

If `send()` loses the response, query `await desktop.command(submission.id)`
first. Only if the ledger reports not-found and the desktop generation is
unchanged may an application explicitly call `submission.send()` again; it
reuses the exact request ID, command ID, and serialized envelope. Its
diagnostic forms redact the command body and lease identity.

`handle.refresh()` and `handle.waitUntilTerminal()` only read/wait for an
existing ID. `AbortSignal` stops the local request; it does not cancel the
server command. Call `handle.cancel()` explicitly when remote cancellation is
intended.

## Lease lifecycle

`desktop.acquireControl()` returns an explicit lease with `renew()` and
`release()`. An explicitly acquired lease never auto-renews; `await using`
guarantees awaited release at scope exit but still leaves renewal policy to the
caller:

```ts
await using lease = await desktop.acquireControl(60_000);
await lease.renew();
await lease.keyboard.chord(["ControlLeft", "L"]);
```

`desktop.withControl()` is the deliberate scoped convenience: it starts renewal
at half the requested or observed TTL, stops renewal when the callback settles,
and then awaits release. A sticky renewal failure fences later lease operations.
If both the callback and release fail, the callback failure remains primary.
Garbage collection cannot promise asynchronous renewal or release.

## Queries and race-free waits

Use server selectors and waits instead of sleeps:

```ts
const editorSelector = {
  type: "predicate",
  predicate: {
    type: "text",
    field: "title",
    matcher: { type: "contains", value: "Editor", case_sensitive: false },
  },
} as const;

const page = await desktop.windows.query({
  selector: editorSelector,
  order: "creation_ascending",
});

const editor = await desktop.windows.one(
  editorSelector,
  "creation_ascending",
);

const settled = await desktop.windows.wait({
  target: { type: "selector", selector: editorSelector, quantifier: "any" },
  predicate: { type: "exists" },
  timeout_ms: 10_000,
});
```

`desktop.accessibility.query()` and `.wait()` use the same generation fencing.
Selectors are emitted exactly as the public v1 schema defines them. Unknown
additive response metadata remains available on the returned objects.

`windows.one()` and `accessibility.one()` require exactly one match.
`windows.first()` and `accessibility.first()` deliberately select index zero
and therefore require an explicit stable order. Their handles preserve the
identity observed at creation. Once a snapshot reports `stale_reference`, the
handle remains stale even if the server later reuses an underlying bus identity.
Relocation is always explicit and returns a new handle; it never mutates the
old identity. Window handles expose activation, close, geometry, state,
workspace, stacking, capture, and reference waits. Element handles expose
invoke, focus, value/text replacement, offset text insertion, selection,
scrolling, physical click, and reference waits; every operation rechecks the
shared generation fence before submitting.

## Exact 64-bit counters

Precision-sensitive `uint64` values are canonical decimal strings in wire
interfaces. They are never represented as `number`:

```ts
import { decodeUInt64, encodeUInt64 } from "@xenoteer/sdk";

const sequence = decodeUInt64("9007199254740993"); // bigint
const wire = encodeUInt64(sequence);               // exact decimal string
```

Noncanonical text, JSON numbers, overflow, signs, whitespace, and leading zeros
are rejected.

## Installed behavior example

The npm tarball ships `examples/phase6-behaviors.mjs`. Run that file from the
installed package, not from a repository checkout:

```sh
node node_modules/@xenoteer/sdk/examples/phase6-behaviors.mjs
```

It requires `XENOTEER_API_BASE`, `XENOTEER_TOKEN`,
`XENOTEER_EXPECTED_INSTALL_ROOT`, `XENOTEER_EXPECT_AUTH_FAILURE`, and
`XENOTEER_QUICKSTART_LANGUAGE`. The repository gate supplies those values and
an exact derived-image GTK fixture. The example proves the same ten behaviors
as the Rust and Python artifact examples: capabilities, scoped launch/lease,
exact resolution, semantic invoke, smooth physical click, Unicode strategy
evidence, screenshot after a real failed postcondition, reconnect by known
command ID, stale reference after restart, and view-only browser ticket.

## Events and authentication

`decodeEventMessage()` returns typed known topics and an `UnknownEvent` for
future topics. Both expose the complete immutable raw message, including the
exact sequence string and payload.

The control WebSocket requires an HTTP `Authorization` header. The Node built-in
`WebSocket` cannot set that header, so the SDK does not put the long-lived token
in a URL or subprotocol. Configure one header-capable `webSocketFactory` on the
root connection policy and then open sessions from that retained policy:

```ts
const client = await XenoteerClient.connect({
  baseUrl: "https://127.0.0.1:9443",
  token: async () => process.env.XENOTEER_TOKEN!,
  fetch: boundedTlsFetch,
  webSocketFactory: ({ authorization, url, maxMessageBytes, handshakeDeadlineMs }) =>
    makeBoundedAuthenticatedSocket({
      authorization,
      url,
      maxMessageBytes,
      handshakeDeadlineMs,
    }),
  reconnect: { maxAttempts: 5, initialDelayMs: 100, maxDelayMs: 5_000 },
  webSocketMaxMessageBytes: 1_048_576,
  webSocketHandshakeTimeoutMs: 10_000,
});

await using events = await client.openEventSession();
```

The factory must apply `authorization` only to the upgrade request and redact it
from its own diagnostics. The SDK calls the token provider afresh for every
HTTP attempt, initial WebSocket attempt, and reconnect candidate. The older
`openEventSession(factory, options)` form remains as a compatibility overload;
new code should retain the factory at `connect()`. Opening an event session
without either explicit form fails before credential lookup.

The root numeric WebSocket/reconnect policy is copied and validated before the
status request, so later mutation of the caller's option objects cannot change
live behavior. Per-session options remain available for the compatibility
surface and are themselves copied by the session. `clientName`,
`clientVersion`, and the exact negotiated minor are retained in every reconnect
hello. Reconnect never replays an HTTP effect. Authentication, permission,
protocol, and desktop-generation welcome failures are permanent; transient
handshake failures consume only the configured bounded retry budget. Every
failed candidate socket is closed before another candidate is requested.

The local event queue is bounded and closes with an explicit terminal reason
on overflow. The session waits for a validated welcome, correlates subscribe
acknowledgments, supervises application heartbeats, and reconnects with replay
only for the same desktop generation. History loss or generation change emits
`resync_required` and requires fresh authoritative snapshots.

Every event, replay completion, and resynchronization marker must match the
active subscription request ID, desktop ID, desktop generation, and exact topic
filter (an empty filter means all authorized topics). An exact duplicate
sequence is ignored; a lower sequence is a `sequence_regression` resync
boundary. `ReplayCompleteEvent` and `ResyncRequiredEvent` use reserved bounded
queue capacity, so an ordinary-event backlog cannot hide authoritative
continuity evidence.

The injected WebSocket implementation itself must enforce the supplied
`maxMessageBytes` ceiling and upgrade deadline. The SDK additionally checks
text/binary byte length before UTF-8/JSON decoding and applies the smaller of
its configured limit and the server welcome limit.

## Transport and artifacts

HTTPS is accepted for ordinary origins. Plaintext HTTP is deliberately limited
to numeric loopback addresses such as `127.0.0.1` or `[::1]`; `localhost` is not
accepted because name resolution is mutable.

Injected `fetch` and `webSocketFactory` adapters are one paired connection-policy
unit. Deriving same-origin `wss://.../v1/ws` from the HTTPS base URL copies only
the origin. It does **not** copy or infer CA roots, mTLS identities, proxy or
`no_proxy` policy, certificate pins, DNS policy, socket agents, or any other
transport setting from `fetch`. Configure equivalent policy explicitly in both
adapters. The SDK deliberately does not introspect or duplicate secret TLS
material.

The SDK borrows the token provider, safe-log hook, `fetch`, and WebSocket factory
callbacks for the client lifetime; callers retain those callback objects and
must keep their captured resources valid. Each socket returned by the factory
is SDK-owned. The SDK closes it after a failed handshake and when the event
session closes. A factory must return a new socket for every invocation and must
not share or close that socket behind the SDK.

`desktop.artifacts`, `desktop.clipboard`, `desktop.capture`,
`desktop.applications`, and `desktop.viewer` expose the remaining v1 domains.
Artifact transfers are bounded, generation-scoped, length-checked, and
SHA-256-checked. Clipboard values, command bodies, artifact IDs, lease IDs, and
viewer tickets have redacted diagnostic forms. Viewer tickets are returned only
through `consumeSecret()` for deliberate one-time WebSocket admission and are
never appended to the viewer URL.

Large clipboard writes use a temporary artifact. After a caller observes the
command's terminal result, the command handle performs best-effort deletion.
Cleanup failure never rewrites a successful command result; it is exposed as a
redacted `cleanupError`, while the server-side artifact expiry remains the
reliable fallback if the process exits or the handle is abandoned.

## Credential handling

Tokens must be 32–1024 bytes of canonical `token68` text. `BearerToken`,
`XenoteerClient`, SDK errors, and `redactedClientOptions()` do not expose the
credential in string, JSON, or Node inspection output. Avoid embedding tokens in
source, URLs, exception messages, or application telemetry.

The optional `log` hook receives frozen metadata with a closed schema:
`operation`, `outcome`, and only bounded attempt/method/route-template/status/
byte-count/stable-error-code fields. It never receives headers, credentials,
raw URLs or paths, query values, IDs, tickets, bodies, server prose, close
reasons, callback errors, or transport exception sources. Unknown paths become
the literal route `unknown`. Each actual HTTP, artifact, or WebSocket attempt
emits one `started` event and one terminal event. Streamed downloads emit
`succeeded` only after bounded EOF; early return, cancellation, and stream error
emit `failed`. Hook exceptions are swallowed and cannot alter, retry, cancel,
or replace the underlying operation.

## Development

```sh
npm ci
npm test
npm run conformance
npm run verify-pack
```

The tests use mock `fetch` responses and cover negotiation, redaction,
single-attempt mutation behavior, explicit lease lifecycle, smooth motion,
generation-fenced queries, unknown events, and exact 64-bit boundaries.
The conformance command runs all 73 frozen Xenoteer v1 SDK cases with no skips
through the real public SDK helpers; it does not claim live-container
integration. The package verifier runs two independent `npm pack --dry-run`
passes, requires deterministic metadata and an exact file allowlist, and
rejects server sources, symlinks, runtime dependencies, or Business Source
License material from the Apache-2.0 client package.
