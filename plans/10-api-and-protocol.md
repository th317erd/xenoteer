# HTTP and WebSocket API protocol

## 1. Protocol goals

The protocol must make side effects, retries, partial completion, event gaps,
and desktop restarts explicit. It is designed for generated/handwritten SDKs,
not as a thin dump of Rust structs or x11rb/AT-SPI calls.

Release one uses:

- HTTP for health, status, snapshots, command submission/result lookup, viewer
  tickets, and artifact bodies;
- one WebSocket for low-latency command submission, results, events, and
  application heartbeat;
- JSON UTF-8 application messages;
- binary HTTP bodies for PNG/artifacts rather than giant base64 event messages.

## 2. Versioning

### 2.1 URI and message versions

- Stable routes live under `/v1`.
- Every WebSocket hello and command envelope carries `{major, minor}`.
- Major changes may remove/change semantics and use a new URI `/v2`.
- Minor changes are additive: new commands, fields, enum variants, and
  capabilities.
- Server advertises `protocol_min` and `protocol_max`; negotiation selects the
  highest mutually supported minor within major 1.

SDKs MUST tolerate unknown response fields and event topics. Unknown enum values
are preserved as `Unknown(String)` or surfaced as a forward-compatibility error
without corrupting the connection. Servers reject unknown command fields and
command types so client typos never become ignored behavior.

### 2.2 Capabilities

`server.welcome` and `/v1/status` provide a structured set such as:

```text
input.pointer.smooth
input.keyboard.xtest
input.text.clipboard
window.ewmh.activate
accessibility.atspi
capture.root.png
viewer.novnc.view_only
```

Capabilities can be `available`, `degraded`, `unavailable`, or `disabled`, with
safe reason code and backend version. SDK methods may preflight but the server
rechecks at execution; capability is not a permanent promise.

## 3. Authentication and transport

- Bearer authentication is required on all `/v1` routes and WebSocket upgrade.
- `/livez` can be unauthenticated and reveals only alive/not alive.
- `/readyz` can be unauthenticated but reveals only ready/not ready; detailed
  capability reasons require auth.
- Default startup creates a random 256-bit token at a root/runtime-prepared 0600
  file when no configured token file exists. It logs the path, never the token.
- Static tokens are stored as keyed hashes/constant-time compared and map to a
  principal/capability set. Token rotation can overlap old/new tokens for a
  bounded grace period.
- Authentication disabled is allowed only with an explicit insecure-development
  flag and a loopback listener; startup refuses the combination with non-loopback.
- Production external access uses TLS directly or a trusted same-host/sidecar
  reverse proxy. Proxy headers are accepted only from configured proxy addresses.

WebSocket browser upgrades enforce an Origin allowlist. Non-browser SDKs may omit
Origin. CORS is disabled except configured exact origins; wildcard with
credentials is forbidden.

## 4. Resource model

One release-one daemon exposes one desktop but retains `DesktopId` in paths and
envelopes for future/fleet consistency. Canonical resources:

- desktop status/generation;
- controller lease;
- command and result;
- window/element/process references;
- event stream;
- screenshot/artifact;
- viewer ticket.

References are opaque structured values returned by the server. Clients should
not construct identity hashes or object paths manually.

## 5. HTTP routes

### Health/status

```text
GET  /livez
GET  /readyz
GET  /v1/status
GET  /v1/capabilities
```

### Lease

```text
POST   /v1/desktops/{desktop_id}/lease
POST   /v1/desktops/{desktop_id}/lease/{lease_id}/renew
DELETE /v1/desktops/{desktop_id}/lease/{lease_id}
GET    /v1/desktops/{desktop_id}/lease
```

GET returns only the caller's full lease; other principals receive occupied/
expiry metadata without owner/token details.

### Commands

```text
POST   /v1/desktops/{desktop_id}/commands
GET    /v1/desktops/{desktop_id}/commands/{command_id}
DELETE /v1/desktops/{desktop_id}/commands/{command_id}
GET    /v1/desktops/{desktop_id}/commands/{command_id}/wait?timeout_ms=...
```

POST returns `202 Accepted` with current `CommandResult`, `Location`, and
`Retry-After` where useful. If already terminal under the same command ID/hash,
return `200`. Different hash -> `409 command_id_conflict`.

DELETE requests cooperative cancellation: `202` if accepted, `200` if already
terminal, `409` when no safe cancellation is supported and the effect is in an
atomic stage. It never promises undo.

### Read-only snapshots/query conveniences

```text
GET  /v1/desktops/{desktop_id}/windows
GET  /v1/desktops/{desktop_id}/windows/{reference_token}
POST /v1/desktops/{desktop_id}/elements/query
GET  /v1/desktops/{desktop_id}/elements/{reference_token}
GET  /v1/desktops/{desktop_id}/processes
```

Complex selectors use POST query bodies even though read-only, to avoid URL
length/encoding limits. Reference-token is a short URL-safe serialization or a
server-issued lookup token; full refs can also be supplied in command JSON.

### Capture/artifacts/viewer

```text
POST   /v1/desktops/{desktop_id}/screenshots
POST   /v1/artifacts?purpose=clipboard_input
GET    /v1/artifacts/{artifact_id}
DELETE /v1/artifacts/{artifact_id}
POST   /v1/desktops/{desktop_id}/viewer-tickets
GET    /viewer/
GET    /v1/viewer/ws?ticket=...
```

Screenshot POST can return image bytes directly for small synchronous requests
or `202` artifact reference. The `Accept` header selects `image/png` or JSON
metadata. Artifact GET has content type/length/hash, `Cache-Control: private,
no-store` by default, and authorization checks.

Artifact upload is a raw binary HTTP body, never multipart or base64. Release one
accepts only `purpose=clipboard_input`, requires `clipboard:write`,
`Content-Type`, and `Content-Length`, and permits an optional caller SHA-256 that
the server verifies. The server streams to a private temporary file under both
the 16 MiB clipboard limit and global artifact quota, fsyncs/closes, hashes, then
atomically publishes an immutable `ArtifactRef`; partial/mismatched uploads are
deleted. It returns `201 Created`. The ref is principal/desktop/purpose scoped,
expires under the one-hour private-artifact TTL, and cannot be used as an
application file path. Command execution revalidates the ref. This is the only
larger-body exception to the 1 MiB request-message default.

Artifact authority follows purpose rather than treating an artifact ID as a
capability: clipboard output requires `clipboard:read`, screenshots require
`capture:read`, and trace/support artifacts require `artifact:read`. The
principal that created a clipboard-input artifact can delete it under
`clipboard:write`; that grant does not permit listing or reading other
artifacts. Every GET/DELETE also checks desktop, principal/share policy, expiry,
and generation where applicable.

Viewer ticket is single-use or short TTL (initially 60 seconds), scoped to
principal, desktop generation, origin, and view-only permission. It is distinct
from the long-lived API token so browser URLs do not expose API authority.

## 6. WebSocket lifecycle

Endpoint: `GET /v1/ws` upgrade under authenticated request.

### 6.1 Handshake

Client's first application message within 5 seconds:

```json
{
  "type": "client.hello",
  "request_id": "...",
  "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
  "client": {"name": "xenoteer-rust", "version": "..."},
  "resume": {"desktop_id": "...", "event_sequence": 1042}
}
```

Server response:

```json
{
  "type": "server.welcome",
  "protocol": {"major": 1, "minor": 0},
  "connection_id": "...",
  "principal": {"id": "...", "capabilities": ["..."]},
  "desktop": {"id": "...", "generation": "...", "state": "ready"},
  "limits": {"max_message_bytes": 1048576, "heartbeat_ms": 15000},
  "resume": {"status": "replayed|resync_required|not_requested"}
}
```

No commands before welcome. Unsupported version sends structured error then
close code 1002/application-specific code.

### 6.2 Client messages

- `command.submit` with complete envelope;
- `command.cancel`;
- `command.watch`/`command.unwatch` for known IDs;
- `events.subscribe`/`events.unsubscribe` with topic filters and optional replay;
- `lease.acquire`, `lease.renew`, `lease.release` conveniences;
- `client.ping` with nonce/timestamp;
- `snapshot.request` only for bounded small JSON snapshots; large artifacts use
  HTTP.

Every message has `request_id`; server acknowledgments/errors echo it.

### 6.3 Server messages

- `command.accepted`;
- `command.progress` (coalescible, not guaranteed for every step);
- `command.result` terminal/non-coalescible;
- `event` with sequence/topic;
- `events.replay_complete`, `events.resync_required`;
- `lease.state`;
- `server.pong`;
- `server.draining`;
- `error` for connection/message-level problems.

Command errors belong inside terminal `command.result`. A malformed message that
cannot identify a command receives connection-level `error`.

### 6.4 Framing and limits

- Text messages only for v1 application protocol.
- Maximum reassembled message 1 MiB default; fragmented WebSocket frames do not
  bypass it.
- Maximum ordinary HTTP JSON body is also 1 MiB. Screenshot response and
  purpose-bound binary artifact upload limits are enforced by their streaming
  handlers before buffering.
- Invalid UTF-8/JSON/protocol closes after bounded error response.
- Per-message decompression is disabled initially to avoid compression bombs and
  secret cross-compression concerns. Revisit with explicit limits.
- RFC Ping/Pong handled by library; additionally application ping every 15 s and
  stale after 45 s proves writer/reader/event-loop health through proxies.
- One writer task owns the socket sink.
- Outbound queues reserve capacity for terminal results, resync, and shutdown.

## 7. Command idempotency and retry semantics

Mutating command IDs are mandatory. Read-only snapshot requests may still carry
one for tracing but can opt out of ledger retention.

Canonical request hash is computed after deserialization/normalization using a
deterministically serialized typed command (all maps are ordered), plus desktop
generation and behavior-affecting envelope fields. JSON field order/whitespace
does not affect it.

Retry advice:

- `same_command_id`: safe only to retrieve/submit exact command within ledger
  retention; it returns the existing execution/result.
- `after_resync`: obtain current generation/reference then create a new command.
- `after_backoff`: no effect occurred and transient resource failed; new command
  ID is allowed.
- `never`: partial/ambiguous effect or validation/policy failure.

HTTP `Idempotency-Key`, if accepted, MUST equal textual `command_id`; body remains
authoritative. SDKs do not generate a new ID on transport retry.

The ledger is in-memory in release one. Desktop/daemon restart invalidates old
generation and the server cannot dedupe prior effects. SDK reconnect first checks
generation and reports `outcome_unknown_after_server_restart`, never replays.

## 8. Deadlines and timeouts

Client provides absolute UTC deadline or relative timeout (SDK converts to
deadline using server time offset conservatively). Server caps by operation and
uses monotonic timers after admission.

Distinguish:

- queue deadline;
- execution deadline;
- backend call timeouts;
- postcondition timeout;
- HTTP long-poll timeout (does not cancel command);
- WebSocket/session heartbeat.

HTTP request cancellation/disconnect does not cancel an accepted command. Only
explicit command cancel does. This prevents proxy timeouts from leaving unclear
implicit policy.

## 9. Problem details and status mapping

HTTP errors use `application/problem+json` modeled on RFC 9457:

```json
{
  "type": "https://xenoteer.dev/problems/stale-reference",
  "title": "Stale reference",
  "status": 409,
  "code": "stale_reference",
  "detail": "The referenced window no longer identifies the observed window.",
  "instance": "urn:xenoteer:request:...",
  "retry": "after_resync",
  "effect_stage": "none",
  "desktop_generation": "...",
  "details": {"resource": "window"}
}
```

Mapping:

- 400 malformed/validation/version within supported route;
- 401 missing/invalid authentication;
- 403 authenticated but capability/scope denied;
- 404 resource not found only when nonexistence may be disclosed;
- 409 lease/dedupe/stale/ambiguous state conflicts;
- 410 artifact/reference explicitly expired where useful;
- 413 message/artifact too large;
- 422 structurally valid but target operation unsupported/invalid;
- 429 rate/concurrency/queue resource exhaustion with Retry-After;
- 500 internal invariant (safe opaque detail);
- 502 external backend protocol failure;
- 503 desktop not ready/degraded required capability;
- 504 backend/postcondition timeout where no more specific command terminal form.

Do not expose Rust type names, source paths, raw upstream messages, environment,
or secrets. Problem `detail` is safe static/parameterized text.

## 10. Pagination and consistency

Window/process lists use bounded limit/cursor with `snapshot_sequence`. Element
queries use query budgets and continuation only when stable. Cursor is opaque,
signed/MACed or server-side, expires quickly, and binds desktop generation,
principal, query hash, and ordering.

If model revision changes incompatibly between pages, return
`pagination_snapshot_expired` and require restart. Do not return duplicates/
omissions while claiming one snapshot.

## 11. Rate and resource limits

Apply independently:

- HTTP requests per principal/IP;
- WS messages per connection/principal;
- concurrent commands per principal/global;
- one input lease/serialized input queue;
- semantic query concurrency/node/byte budgets;
- screenshot concurrency/pixels/bytes;
- artifact bytes/count/retention;
- application launch rate/process count/output bytes;
- event subscriptions/replay/output queue;
- clipboard bytes/transfers.

Limit errors reveal the limit category and retry delay, not other tenant activity.
Static token auth is for trusted operators in release one; still enforce limits
because accidental loops are common.

## 12. API conformance tests

- schema golden/round-trip and unknown-field rejection;
- every error code/status/problem serialization;
- version negotiation and additive unknown response fields;
- duplicate command same/different hash, concurrent duplicate race, in-progress
  watch, TTL eviction;
- disconnect/reconnect before acceptance, after acceptance, after effect, after
  terminal result;
- old generation retry produces unknown/stale, never re-executes;
- WS fragmented oversize, invalid UTF-8/JSON, compression disabled, slow writer,
  heartbeat timeout, orderly close;
- event replay/resync and high-priority result delivery under damage flood;
- auth missing/invalid/scope/object capability, origin/CORS/proxy trust;
- request timeout does not implicitly cancel command;
- viewer ticket scope/single-use/expiry/origin/generation;
- artifact authorization/range/content headers/expiry;
- OpenAPI/JSON Schema examples compile in all SDKs.

## 13. Primary references

- [RFC 6455 WebSocket Protocol](https://www.rfc-editor.org/info/rfc6455/)
- [RFC 9110 HTTP idempotent method semantics](https://www.rfc-editor.org/info/rfc9110/)
- [RFC 9457 Problem Details for HTTP APIs](https://datatracker.ietf.org/doc/html/rfc9457)
- [Axum WebSocket API](https://docs.rs/axum/latest/axum/extract/ws/)
- [OWASP API Security Top 10](https://owasp.org/www-project-api-security/)
- [OWASP API authentication guidance](https://owasp.org/API-Security/editions/2023/en/0xa2-broken-authentication/)
