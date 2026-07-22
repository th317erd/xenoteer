# Xenoteer public API v1.0

This directory is the client-facing Phase 4 wire contract. The canonical typed
JSON shapes remain the generated JSON Schemas in [`schemas/v1/`](../../../schemas/v1/);
the files here describe how those shapes are transported.

- [`openapi.json`](openapi.json) documents the implemented HTTP endpoints.
- [`websocket.md`](websocket.md) inventories WebSocket application messages and
  separates implemented v1.0 messages from planned extensions.
- [`examples/`](examples/) contains complete payloads with one consistent set of
  identifiers, suitable for SDK fixtures and documentation tests.

Run the dependency-free static check from the repository root:

```sh
python3 scripts/api/validate-docs.py
```

## Contract rules

All `/v1` HTTP routes and the `/v1/ws` control upgrade require exactly one
`Authorization: Bearer <token>` header except the viewer gateway WebSocket. The
gateway consumes a short-lived, origin-bound, single-use viewer ticket from the
`Sec-WebSocket-Protocol` header and does not accept the long-lived API token.
`/livez`, `/readyz`, and the hardened viewer page/static assets are deliberately
public. Ordinary JSON and reassembled control-WebSocket messages are capped at
1 MiB by default.

Each configured token maps to one authenticated principal and a closed set of
grants. The accepted Phase 4 strings are `desktop:status`, `desktop:observe`,
`input:control`, `application:launch`, `application:terminate`, `window:control`,
`clipboard:read`, `clipboard:write`, `capture:read`, `artifact:read`,
`artifact:delete`, and `viewer:read`. Omitting the configuration grants list
preserves the operator-compatible default of all twelve; deployments should
specify the minimum set needed. Unknown or duplicate grants fail configuration
rather than being ignored.

Physical pointer/keyboard/reset and all text-insertion commands require the
caller-owned lease ID and current desktop generation. Other commands remain
generation-fenced but do not acquire authority from the physical-input lease.
Every command has a caller-generated UUIDv4 or UUIDv7 `command_id`. Repeating a
command is safe only with the exact same ID and canonical behavior; a changed
body with the same ID returns `command_id_conflict`. An optional HTTP
`Idempotency-Key` must exactly equal the textual `command_id`.

Accepted work is independent of the HTTP request. A timed-out submit response is
`request_outcome_unknown`; retrieve or resubmit only the exact command ID. A
long-poll timeout returns `202` with the current nonterminal result and does not
cancel the command. Command waits are separately admitted at no more than eight
per principal and 64 globally (or one fewer than the configured HTTP concurrency
limit, whichever is smaller), preserving an ordinary-control slot. Admission
exhaustion returns `429` with `Retry-After`; clients must not open unbounded
parallel waits.

## Implemented HTTP surface

The checked-in [`openapi.json`](openapi.json) is authoritative for methods,
parameters, typed schemas, responses, and transport headers. Its path set is
automatically compared with the literal server router paths by
`scripts/api/validate-docs.py`.

| Surface | Implemented routes | Required authority |
| --- | --- | --- |
| Discovery/control | `/v1/status`, `/v1/capabilities`, lease routes, command submit/get/wait/cancel, `/v1/ws` | Per-operation status/observe/input/application/window/clipboard grants |
| Windows | `GET /v1/desktops/{desktop_id}/windows`, `POST .../windows/query`, `POST .../windows/resolve`, `POST .../windows/wait`, `GET .../windows/{reference_token}` | `desktop:observe` |
| Clipboard | `POST /v1/desktops/{desktop_id}/clipboard/read` | `clipboard:read` |
| Screenshots | `POST /v1/desktops/{desktop_id}/screenshots` | `capture:read` |
| Artifacts | `POST /v1/artifacts`, `GET/DELETE /v1/artifacts/{artifact_id}` | Purpose-specific; see below |
| Viewer | `POST /v1/desktops/{desktop_id}/viewer-tickets`, `/viewer/...` static resources, `GET /v1/desktops/{desktop_id}/generations/{desktop_generation}/viewer/ws` | `viewer:read` for ticket issue; one-time ticket for gateway |

Window list, query, resolve, snapshot, and wait responses all come from a
generation-fenced authoritative model revision. Reference tokens and cursors
are opaque, principal-bound, bounded, and expiring; an XID alone is never a
stable public identity. A wait timeout or resynchronization condition is carried
in a successful `WindowWaitResult`, while admission exhaustion is `429`.

Clipboard reads may return bounded inline content or a private artifact
reference. Screenshot capture always returns validated metadata pointing to a
private screenshot artifact; exact window targets are revalidated immediately
before capture.

Artifact upload is a raw streaming `POST /v1/artifacts?purpose=clipboard_input`
with a single non-multipart `Content-Type`, an exact positive `Content-Length`,
and optional lowercase `x-content-sha256`. It requires `clipboard:write` and is
bound to the current ready desktop generation. Downloads require both
`desktop_id` and `desktop_generation` query parameters and authorize by stored
purpose: `clipboard:read` for clipboard output, `capture:read` for screenshots,
or `artifact:read` for action traces/support bundles. Clipboard-input artifacts
are command inputs and cannot be downloaded. One RFC-style single byte range is
supported on the same GET route (`200`, `206`, or `416`); multipart ranges are
not supported. Delete authority is purpose- and owner-aware as described in the
OpenAPI operation and is never implied by knowing an artifact ID.

The public viewer shell contains no credential and accepts no query string.
First issue a ticket using an allowlisted `Origin`, then present exactly the
`binary` and `xenoteer.ticket.<secret>` WebSocket subprotocols with the same
origin to the generation-specific gateway route. Never put the ticket in a URL
or log it. The gateway accepts binary view-only traffic; browser input,
clipboard, and resize are disabled.

## Commands carried by the command endpoint

Phase 4 does not add standalone mutation routes for window control, clipboard
writes, or text insertion. They are variants of the existing authenticated
`POST /v1/desktops/{desktop_id}/commands` envelope and use the same idempotent
command lifecycle:

- `pointer_move`, `pointer_move_relative`, current/root `pointer_click`,
  `pointer_drag`, `pointer_scroll`, `pointer_button_down`, `pointer_button_up`,
  `keyboard_key_down`, `keyboard_key_up`, `keyboard_press`, `keyboard_chord`,
  `keyboard_sequence`, and `input_reset` require `input:control` and the
  caller-owned control lease. A window-targeted `pointer_click` additionally
  requires `window:control`.
- `application_launch` requires `application:launch`; `process_terminate`
  requires `application:terminate`; `desktop_probe` and `process_status`
  require `desktop:observe`.
- `window_activate`, `window_close`, `window_set_state`, `window_minimize`,
  `window_move_resize`, `window_move_to_workspace`, and `window_stack` require
  `window:control` and exact generation-bound window references.
- `selection_set` and `selection_clear` require `clipboard:write`.
- `text_insert` always requires the caller-owned control lease. Physical
  strategies require `input:control` plus `window:control`; clipboard and auto
  strategies require those two grants plus `clipboard:write`.

The generated [`command-envelope.json`](../../../schemas/v1/command-envelope.json)
is the canonical closed union for command payload fields. No route should be
inferred from a command variant name.

The initial Rust SDK accepts plaintext HTTP only for numeric loopback addresses
such as `127.0.0.1` or `[::1]`; hostnames, even `localhost`, and non-loopback
addresses are rejected. Deployments that terminate TLS or expose the service
must use an external authenticated gateway until a TLS SDK transport is added.

HTTP failures use `application/problem+json` in the RFC 9457 shape. The current
transport emits the required `details` member as `{}`. The shared public Problem
schema permits a bounded nonempty details map for future typed errors and command
terminal errors; clients must tolerate safe extension keys.

## License boundary

These public interoperability artifacts are Apache-2.0 as described in
[`../NOTICE`](../NOTICE) and [`../LICENSE`](../LICENSE). The
server/runtime implementation remains under the repository Business Source
License.
