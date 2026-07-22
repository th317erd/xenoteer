# Xenoteer public API v1.0

This directory is the client-facing Phase 3 wire contract. The canonical typed
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

All `/v1` HTTP routes and the `/v1/ws` upgrade require exactly one
`Authorization: Bearer <token>` header. `/livez` and `/readyz` are deliberately
public and coarse. Ordinary JSON and reassembled WebSocket messages are capped
at 1 MiB by default.

Each configured token maps to one authenticated principal and a closed set of
grants. The only accepted Phase 3 grant strings are `desktop:status`,
`desktop:observe`, `input:control`, `application:launch`, and
`application:terminate`. Omitting the configuration grants list preserves the
operator-compatible default of all five; deployments should specify the minimum
set needed. Unknown or duplicate grants fail configuration rather than being
ignored. Discovery needs `desktop:status`; command lookup/watch needs
`desktop:observe`; lease and raw input need `input:control`; registered launches
and managed termination use their matching application grants.

Input commands require the caller-owned lease ID and current desktop generation.
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
