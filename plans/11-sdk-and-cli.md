# SDKs and diagnostic CLI

## 1. Developer-experience goal

The SDK should feel as coherent as Puppeteer while preserving desktop realities:
windows are not pages, AT-SPI elements are not DOM nodes, physical input can
partially occur, and references can become stale after restarts/navigation.

Rust ships first as the reference SDK. TypeScript follows from the stable v1
schema, then Python. All are asynchronous first and behaviorally conformant.
They share generated protocol types/test vectors but use idiomatic public APIs.

## 2. Package boundaries and licenses

- Rust `xenoteer-sdk`: Apache-2.0 package, separate from BSL server crates.
- `@xenoteer/sdk`: Apache-2.0 npm package.
- `xenoteer`: Apache-2.0 Python package.
- JSON schemas/OpenAPI/protocol examples: Apache-2.0 in a clearly marked tree.
- `xenoteerctl`: server repository tool under repository license unless later
  split; it consumes the public Rust SDK only.

Server implementation types/code are never generated into SDK packages. The
license boundary is enforced by package manifests, headers, CI package-content
inspection, and the dependency direction (SDK -> protocol only).

## 3. Shared object model

```text
XenoteerClient
  -> Desktop
       -> Mouse
       -> Keyboard
       -> Clipboard
       -> Windows -> Window
       -> Accessibility -> Element
       -> Applications -> Process
       -> Capture
       -> Viewer
  -> EventStream
  -> CommandHandle<T>
```

Objects are cheap clients around immutable references and a transport handle.
They do not contain server-side live object proxies. Every method sends the
full generation-bound reference so stale behavior is consistent.

## 4. Connection and session

### 4.1 Client options

- base HTTP/WS URL;
- token string or async token provider;
- TLS roots/client identity options using platform conventions;
- connect/request defaults;
- user-agent/client name/version;
- proxy setting where language ecosystem supports it;
- reconnect/backoff policy;
- safe logging hook with redacted headers/bodies;
- protocol minor range.

Tokens never appear in Debug/repr, exception text, URLs, or telemetry. Viewer
tickets may appear in an in-memory URL only long enough to connect and should be
redacted from logs.

### 4.2 Connect

`connect()`:

1. GET status/establish WS;
2. negotiate protocol;
3. store desktop ID/generation/capabilities/limits/server clock observation;
4. start exactly one reader, writer, heartbeat, and reconnect supervisor;
5. restore event subscriptions or emit local `resync_required` callbacks;
6. never acquire an input lease automatically unless requested by a higher-level
   convenience context.

`close()` stops new SDK calls, optionally releases owned lease, waits for writer
close within a bound, and terminates tasks. It does not cancel accepted commands
unless the caller explicitly requests that policy.

### 4.3 Reconnect

Use capped exponential backoff with jitter for transport, not command replay.
After reconnect:

- if desktop generation matches, watch known accepted command IDs and request
  event replay;
- if generation differs, mark all handles stale and resolve uncertain commands
  with `OutcomeUnknownAfterRestart`;
- lease status is re-queried; never assume the old token remains valid;
- event replay failure emits resync and snapshot helpers.

## 5. Command handles

Every mutation creates or accepts a caller-supplied command ID and returns:

```text
CommandHandle<T>
  id
  accepted()
  status()
  result()/await
  cancel()
  progress stream
```

In languages with awaitable customization, `await handle` returns terminal `T`.
The handle retains result watchers independently of the original WS request.

SDK retry behavior:

- Transport failure before server acceptance is unknown until GET/result lookup
  by the same command ID.
- Resubmit only the exact command with same ID when server generation and ledger
  allow it.
- Never create a fresh command ID automatically for input/launch/close.
- Surface `effect_stage`, cleanup result, completed count, and server retry advice
  on exceptions.

Read-only snapshot calls may use ordinary request retry after transport policy if
the server has not returned data, but generation changes still invalidate refs.

## 6. Lease ergonomics

Low level:

```text
lease = await desktop.acquire_control(ttl=60s)
await lease.renew()
await lease.release()
```

Idiomatic scoped form:

```text
async with desktop.control() as control:
    await control.mouse.move(...)
    await control.keyboard.press(...)
```

Rust uses an async scope/helper because `Drop` cannot await a release. Drop may
schedule a best-effort release through a live runtime but documentation states
that explicit `release().await`/scope completion is the guarantee. TypeScript
`using`/Python async context managers similarly release explicitly.

Lease renewal task starts only for an acquired scoped lease, stops on scope exit,
and reports renewal failure immediately to subsequent control calls. It does not
hide a revoked lease behind local retries.

## 7. Input APIs

Examples of stable shape:

```text
await control.mouse.move(120, 300, mode="smooth")
await control.mouse.click(120, 300, button="left")
await control.mouse.drag(start=(10,10), end=(500,400), duration=500ms)
await control.mouse.scroll(y=-3, interval=16ms)

await control.keyboard.press("Enter")
await control.keyboard.chord(["ControlLeft", "L"])
await control.keyboard.insert_text("Hello, 世界", strategy="auto")
```

Method options expose meaningful policy—mode, duration, endpoint requirement,
timeouts, postconditions—not raw XTEST fields by default. Raw keycode/button
methods live under `control.raw` and require server capability.

Results contain requested/observed state and selected text strategy. Convenience
methods do not discard warnings.

Clipboard APIs are explicit about selection, representation, and restoration:

```text
value = await desktop.clipboard.get(selection="clipboard", as="text")
await control.clipboard.set_text("...")
result = await control.keyboard.insert_text("...", strategy="clipboard")
```

The SDK sends values through 256 KiB inline. Above that threshold it streams a
private `clipboard_input` artifact, checks the returned length/hash, and submits
the command by reference. Clipboard output artifacts are downloaded as bounded
streams and exposed as bytes/async readers according to language conventions.
Temporary input artifacts are deleted best-effort only after a terminal command
result; expiry is the reliable cleanup. SDK retries reuse the same artifact ref
and command ID while both remain valid, never upload a second payload after an
ambiguous effect merely to replay the command.

## 8. Windows and elements

### 8.1 Query and strictness

```text
windows = await desktop.windows.query(WindowSelector(title_contains="Editor"))
window = await desktop.windows.one(selector)  # exact-one or AmbiguousTarget
element = await window.accessibility.one(ElementSelector(role="button", name="Save"))
```

`one` never picks arbitrary first. Explicit `.first(order_by=...)` is available
when wanted. Selectors are typed builders plus serializable forms.

### 8.2 Handles

`Window` exposes snapshot/refresh/activate/close/move/resize/state/capture/waits
and a scoped accessibility query. `Element` exposes snapshot/refresh/invoke/
physical_click/focus/value/text/selection/scroll/waits.

Names make strategy visible:

- `element.invoke()` is semantic;
- `element.click()` is physical and may require window activation;
- optional `element.activate(strategy="auto")` is a convenience that returns the
  chosen path prominently.

After `StaleReference`, the handle remains stale. `relocate(selector)` is an
explicit new query returning a new handle; no method mutates the old handle to a
new target.

## 9. Waits and postconditions

Avoid sleep-centric examples. Provide:

- `window.wait_for(...)`;
- `element.wait_for(...)`;
- `desktop.wait_for_window(selector)`;
- `events.wait(predicate)`;
- command option `postcondition` for server-side race-resistant checks;
- local helper composing multiple server predicates with one deadline.

Client-side waits subscribe, snapshot, and handle `resync_required`; they do not
assume every event arrives. Server-side postconditions are preferred around
mutations because they share the desktop observation sequence and survive a
brief client disconnect.

All timeouts use duration types appropriate to language and reject negative/
unreasonably huge values before sending.

## 10. Events

```text
stream = await desktop.events.subscribe(topics=["window.*", "action.*"])
async for event in stream:
    ...
```

Event stream tracks last sequence and exposes:

- typed known event variants;
- `UnknownEvent(topic, raw)` for forward compatibility;
- `ResyncRequired` with helpers to fetch window/element/status snapshots;
- explicit close/error reason;
- bounded local queue configured by caller, with no hidden unbounded buffering.

If caller does not consume fast enough, SDK applies its own documented
coalescing only for coalescible topics or closes locally; it cannot restore
events the server already dropped.

## 11. Errors

One base SDK error carries:

- stable code/status/request ID/command ID;
- retry advice;
- effect stage and cleanup outcome;
- safe structured details;
- last relevant snapshot/ref where present;
- transport source preserved for diagnostics.

Typed subclasses/enum categories:

- authentication/permission;
- validation/unsupported;
- not found/ambiguous/stale;
- lease/conflict;
- timeout/cancel;
- resource/backpressure;
- backend/server;
- partial effect/outcome unknown.

Do not create hundreds of brittle exception classes for every server code. Rust
uses an exhaustive `ErrorCode` plus helpers; TypeScript/Python provide stable
category classes and retain exact code.

Exceptions/reprs redact tokens, command text payload, clipboard data, and
artifact bytes. Optional debug logging logs sizes/hashes, not bodies.

## 12. Rust SDK specifics

```text
crates/xenoteer-sdk/src/
  lib.rs client.rs connection.rs transport.rs command.rs lease.rs
  desktop.rs mouse.rs keyboard.rs clipboard.rs windows.rs accessibility.rs
  process.rs capture.rs viewer.rs events.rs error.rs options.rs
```

- Async Tokio native; no embedded runtime or blocking facade in release one.
- Builder options use owned values and validate on `build/connect`.
- Public result/selector/envelope types re-export from Apache protocol crate.
- Streams implement `futures_core::Stream` where appropriate.
- `CommandHandle<T>` is cancel-safe: dropping handle stops local watching, not
  server execution.
- Feature flags: `rustls-tls` default, optional native roots; no default OpenSSL.
- Mock transport trait is test-only/publicly limited so SDK conformance tests can
  simulate disconnect/replay without a daemon.

## 13. TypeScript SDK specifics

```text
packages/typescript/src/
  client.ts connection.ts protocol.generated.ts command.ts lease.ts
  desktop/ mouse.ts keyboard.ts windows.ts accessibility.ts ...
```

- ESM-first package requiring Node 22 or newer and tested on every active or
  maintenance LTS line; browser support only
  for viewer/event/read operations unless token exposure policy is accepted.
- AbortSignal cancels local wait and optionally sends explicit command cancel
  only when method option `cancelServerCommand` is true.
- Use `bigint` or validated number/string according to schema for 64-bit counters;
  never silently lose event sequence precision.
- Generated wire interfaces remain internal/advanced; ergonomic classes validate
  and wrap them.
- Reconnect uses WebSocket implementation injection for Node/browser portability.

## 14. Python SDK specifics

```text
packages/python/xenoteer/
  client.py connection.py protocol_generated.py command.py lease.py
  desktop.py mouse.py keyboard.py windows.py accessibility.py ...
```

- asyncio-native with `Requires-Python >=3.11`, tested on 3.11 and the latest
  stable CPython.
- `httpx` async client for HTTP and `websockets` asyncio client for WebSocket;
  both are directly pinned by the SDK lock/test matrix. No synchronous facade
  initially.
- Generated frozen standard-library dataclasses plus explicit converters and
  validators; no Pydantic runtime dependency. Parsing preserves unknown response
  fields in a bounded `extensions` mapping.
- Async context manager for client/lease/event streams.
- `asyncio.CancelledError` of local coroutine does not imply server command
  cancellation; docs and optional flag make this explicit.

## 15. CLI design

Binary `xenoteerctl`, configuration from flags then config file/environment.
Never accept token as a default command-line flag visible in process lists;
prefer token file/stdin/environment with warnings.

Command tree:

```text
xenoteerctl status | doctor | capabilities
xenoteerctl lease acquire|renew|release|show
xenoteerctl mouse move|click|drag|scroll|position|reset
xenoteerctl key press|down|up|chord|text|reset
xenoteerctl clipboard get|set|clear|paste
xenoteerctl window list|find|show|activate|close|move|resize|state|capture|wait
xenoteerctl element query|show|invoke|click|focus|text|value|wait
xenoteerctl app list|launch|terminate|logs
xenoteerctl screenshot
xenoteerctl events watch
xenoteerctl viewer url
xenoteerctl command show|wait|cancel
```

Rules:

- Mutating commands auto-acquire a short lease only with `--with-lease`; default
  requires supplied/current lease so scripts do not steal control.
- `--json` outputs one stable JSON object/JSONL events to stdout; progress/errors
  go stderr.
- Binary screenshot goes to explicit `--output` or stdout only with
  `--output=-`; never mix logs.
- Selectors are repeatable typed flags and `--selector-json` for complex forms.
- Ambiguous selection prints candidates and exits conflict; no first-match.
- `--command-id` supports safe scripted retry; generated ID is printed before or
  with acceptance.
- `doctor --input` is explicit because it causes pointer/key probe effects.

## 16. Documentation and examples

Each SDK ships the same behavior-oriented examples:

1. connect/status/capability check;
2. acquire scoped lease and launch fixture;
3. find exact window/element;
4. invoke semantic action;
5. perform smooth physical click and wait for postcondition;
6. exact Unicode text with strategy result;
7. screenshot on failure;
8. reconnect and resolve a known command ID;
9. handle stale reference after application restart;
10. view-only browser ticket.

Examples run in CI against the same fixture image. Generated API reference is not
a substitute for guides on effects, retry, stale refs, and accessibility gaps.

## 17. Conformance suite

A language-neutral fixture file describes request/response/error/event JSON.
Each SDK must pass:

- schema encode/decode including unknown additive fields/events;
- command-ID retry and conflicting body;
- transport disconnect at all command stages;
- generation change/stale handle;
- lease context renewal/release failure;
- local cancellation versus server cancellation;
- event sequence/replay/resync/slow consumer;
- redaction in repr/error/log;
- selector ambiguity and explicit ordering;
- all quick-start examples against container.

Release SDK versions declare compatible server protocol range. CI tests newest
SDK against oldest supported server fixture and vice versa for additive changes.

## 18. Primary references

- [RFC 6455 WebSocket protocol](https://www.rfc-editor.org/info/rfc6455/)
- [Tokio cancellation-safe channel behavior](https://docs.rs/tokio/latest/tokio/sync/mpsc/struct.Receiver.html)
- [Cargo package manifest/license rules](https://doc.rust-lang.org/cargo/reference/manifest.html)
- [Axum WebSocket split pattern](https://docs.rs/axum/latest/axum/extract/ws/)
- [HTTPX async support](https://www.python-httpx.org/async/)
- [websockets asyncio client](https://websockets.readthedocs.io/en/stable/reference/asyncio/client.html)
