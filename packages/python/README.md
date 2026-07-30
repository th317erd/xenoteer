# `xenoteer`

Asyncio-first Python 3.11+ SDK for the frozen Xenoteer v1 desktop automation
API. The package is independently licensed under Apache-2.0 and contains no
Business Source License server implementation.

## Connect and control

```python
import os

from xenoteer import ClientOptions, XenoteerClient


async def token_provider() -> str:
    return os.environ["XENOTEER_TOKEN"]


async def automate() -> None:
    async with await XenoteerClient.connect(
        ClientOptions(
            base_url="https://127.0.0.1:9443",
            token=token_provider,
        )
    ) as client:
        desktop = client.desktop()  # immutable ID + generation fence

        async with desktop.control(ttl=60) as control:
            submission = control.mouse.move(120, 300, duration=0.35)
            # The ID and exact body exist before network I/O.
            move = await submission.send()
            await move.wait_until_terminal(timeout=10)

            press = await control.keyboard.press("enter")
            await press.wait_until_terminal(timeout=10)
```

`connect()` authenticates `GET /v1/status`, validates the current desktop, and
negotiates the highest shared protocol minor. It never acquires a controller
lease. Desktop, lease, window, element, and command handles remain bound to the
generation in the status snapshot and never silently relocate after restart.

### Connection adapters and ownership

`XenoteerClient.connect()` is the single adapter boundary:

```text
connect(
    options,
    *,
    http_client=None,
    transport=None,
    websocket_factory=None,
    transport_ownership="borrowed",
)
```

`http_client` and `transport` are mutually exclusive and are rejected before
network I/O. Ownership is explicit:

| Connection input | HTTP resource owner | On failed connect | On client close | Event WebSocket |
| --- | --- | --- | --- | --- |
| neither | SDK | SDK closes it | SDK closes it once | reviewed default factory |
| injected `httpx.AsyncClient` | caller | remains open | remains open | explicit paired factory required |
| injected `AsyncTransport`, `borrowed` | caller | remains open | remains open | explicit paired factory required |
| injected `AsyncTransport`, `client` | SDK | closes it once | closes it once | explicit paired factory required |

Every callback is borrowed for the connected client's lifetime. Every socket
returned by a WebSocket factory is SDK-owned and closed exactly once after a
failed handshake, replacement, or session close. An injected
`httpx.AsyncClient` is always borrowed; asking for `transport_ownership="client"`
with one is rejected instead of pretending that ownership changed.
Factory sockets must support weak references, as the supported `websockets`
connection does. This lets the SDK remember that the same live physical socket
was already closed without retaining dead generations or confusing a later
object that reuses its raw runtime ID. An incompatible candidate is rejected
before handshake I/O and closed within the connection deadline.

If status validation or protocol negotiation fails, cleanup of an SDK-owned
transport is capped by five seconds and the configured connect/request
deadlines. Cleanup failure or timeout never replaces the original connection
error. Custom transport cleanup must remain cancellation-cooperative; Python
cannot preempt an arbitrary adapter that suppresses cancellation, so such a
cleanup task may outlive the cap until the adapter eventually returns. Borrowed
transports and injected HTTP clients are never closed by this failure path.

### Reconnect, TLS, and proxy policy

`ClientOptions.reconnect_policy` is one frozen `ReconnectPolicy`. It bounds the
attempt count, initial and maximum exponential delay, and minimum/maximum jitter.
The exact policy, client name/version, negotiated protocol minor, WebSocket URL,
and factory are retained across reconnects. A fresh token is resolved before
every HTTP, artifact, initial WebSocket, and reconnect WebSocket attempt.
Reconnect applies only to the event transport and never replays commands.

`ClientOptions.connect_timeout` (10 seconds by default, at most 60) is one wall
deadline for status negotiation and for each initial/reconnect WebSocket token,
factory, hello send, bounded welcome, and validation sequence. The WebSocket
hello and welcome are each capped at 1 MiB. `request_timeout` also bounds token
resolution before HTTP and artifact adapter I/O. A token source is either a
static string or a genuinely asynchronous provider (`async def` or async
`__call__`); synchronous callables are rejected during option validation before
I/O and are never invoked. Deadlines cancel and await cancellation-cooperative
async providers, including their `finally` cleanup. Python cannot preempt a
provider that blocks the event loop or suppresses cancellation, so providers
must remain non-blocking and propagate `CancelledError`.
`client_name` and `client_version` are nonempty, at most 128 UTF-8 bytes, and
reject Unicode control characters before any I/O.

HTTP and WSS adapters are a paired security policy. Deriving a same-origin WSS
URL does **not** transfer custom CA roots, mTLS identity, proxy or `no_proxy`
rules, certificate pins, DNS resolution, or custom network agents from an HTTP
adapter. Xenoteer deliberately does not inspect or copy secret TLS material.
When HTTP behavior is customized through `http_client` or `transport`, supply a
`websocket_factory` configured with the matching reviewed TLS/proxy policy;
`open_events()` fails explicitly when the pair is missing. The package defaults
use the independently reviewed platform defaults of HTTPX and `websockets`.

### Safe connection logging

`ClientOptions.safe_log_hook` accepts a synchronous callback and rejects an
`async def` hook before I/O. It receives a frozen `SafeLogEvent` with only:
operation, outcome, attempt, method, reviewed route template, status,
request/response byte counts, and stable error code. Operations are closed to
`http.request`, `artifact.upload`, `artifact.download`, `artifact.delete`, and
`websocket.handshake`; outcomes are `started`, `succeeded`, or `failed`.

There is exactly one `started` and one terminal event for each actual attempt,
including token-provider, body, transport, status, header, stream, sink, and
cancellation failures. A download succeeds only after its complete verified
stream reaches the sink. Unknown paths become `unknown`; raw paths, URLs,
queries, IDs, tickets, credentials, bodies, server prose, close reasons,
provider/callback errors, and transport exception text have no field in the
schema. Ordinary hook exceptions are ignored and never retry, cancel, or replace
the transport outcome. Process-control exceptions such as `KeyboardInterrupt`
are intentionally not swallowed.

## No hidden command replay

Every mutation is prepared before HTTP I/O as a redacted `CommandSubmission`.
It retains the exact canonical body and command ID across cancellation or an
ambiguous disconnect. The transport has no retry loop and never invents a
replacement identity:

```python
submission = desktop.submit(
    {"type": "desktop_probe"},
    command_id="40000000-0000-4000-8000-000000000010",
)
known_id = submission.id
known_body = submission.canonical_body
handle = await submission.send()
```

`handle.refresh()` and `handle.wait_until_terminal()` only read or wait on the
existing ledger identity. If submission transport fails, look up `known_id`;
only after a confirmed NotFound may the caller explicitly call
`submission.send()` again with the retained same body and ID. Cancelling an
asyncio task does not send a server cancellation. `await handle.cancel()` is
the only convenience that explicitly requests remote cancellation.

## Explicit lease lifecycle and smooth input

`desktop.acquire_control()` returns a lease with `renew()` and `release()`.
`async with desktop.control()` awaits release even when the body raises. There
is exactly one bounded renewal task while the scoped lease is active; renewal
failure fences subsequent input until `recover()` or reacquisition. Ambiguous
renew/release outcomes require an authoritative `recover()` query. Python
garbage collection cannot promise an async release.

Mouse motion defaults to the server's smooth interpolation curve. Keyboard,
clipboard mutations, clicking, dragging, and scrolling are available only
through an explicit lease:

```python
async with desktop.control(ttl=30) as control:
    await control.mouse.click(400, 220)
    await control.keyboard.chord(["control_left", "l"])
    await control.clipboard.set_text("bounded inline text")
```

## Windows, accessibility, capture, and viewing

Client-authored selector dictionaries are strict frozen-v1 inputs: unknown or
obsolete fields are rejected before I/O. Server-authored response dictionaries
preserve additive fields so newer metadata survives round-trips:

```python
window = await desktop.windows.one(
    {
        "type": "predicate",
        "predicate": {
            "type": "text",
            "field": "title",
            "matcher": {
                "type": "exact",
                "value": "Xenoteer GTK3 Fixture — Main",
                "case_sensitive": True,
            },
        },
    }
)
await (await window.activate()).wait_until_terminal(10)

element = await desktop.accessibility.one(
    {
        "scope": {"type": "desktop"},
        "predicates": [
            {
                "type": "name",
                "matcher": {
                    "type": "exact",
                    "value": "Stable Button",
                    "case_sensitive": True,
                },
            }
        ],
        "order": "object_path_ascending",
        "result_index": None,
    }
)
await (await element.invoke()).wait_until_terminal(10)

screenshot = await desktop.capture.screenshot(include_cursor=True)
ticket = await desktop.viewer.ticket("https://viewer.example")
secret = ticket.expose_ticket()  # expose only at browser bootstrap
```

`windows.one()` and `accessibility.one()` use exact-one resolution. They never
pick an arbitrary first match. Read-only clipboard and capture APIs return the
complete additive server response.

## Exact 64-bit counters

Precision-sensitive unsigned 64-bit values are canonical decimal strings on the
wire. They never pass through `float`:

```python
from xenoteer import decode_uint64, encode_uint64

value = decode_uint64("9007199254740993")  # exact Python int
wire = encode_uint64(value)                # "9007199254740993"
```

JSON numbers, floats, overflow, signs, whitespace, and leading zeros are
rejected for canonical fields. Request references supplied with Python integers
are converted directly to decimal strings without a floating-point conversion.

## Events and credentials

`client.open_events()` authenticates with an HTTP Authorization header. The API
token is never put in the WebSocket URL or subprotocol. The local event queue is
bounded and closes with an explicit backpressure error on overflow.
`decode_event_message()` returns `KnownEvent` for known topics and
`UnknownEvent` for future topics; both preserve a defensive copy of the complete
raw message and exact sequence text.

Tokens must be 32–1024 characters of canonical `token68` text. `BearerToken`,
`ClientOptions`, `XenoteerClient`, and SDK errors redact credentials from repr
and messages. Viewer tickets use an explicit `expose_ticket()` boundary and are
redacted from repr.

HTTPS/WSS is required for non-loopback hosts. Plain HTTP/WS is accepted only for
a numeric loopback address. Artifact streaming validates declared length and
digest before returning success. `Artifacts.download_to()` accepts only a
genuine async sink (`async def` function or async callable object), validated
before adapter I/O. Sink execution shares the request's absolute deadline and
must remain non-blocking and cancellation-cooperative. Use `download_bytes()`
when an in-memory result is sufficient.

## Installed behavior example

Every wheel and source distribution ships
`xenoteer.examples.phase6_behaviors`. It is an executable qualification example,
not a source-tree test:

```sh
PYTHONNOUSERSITE=1 python -m xenoteer.examples.phase6_behaviors
```

The example requires `XENOTEER_API_BASE`, `XENOTEER_TOKEN`,
`XENOTEER_EXPECTED_INSTALL_ROOT`, `XENOTEER_EXPECT_AUTH_FAILURE`, and
`XENOTEER_QUICKSTART_LANGUAGE`. The repository gate supplies those values,
starts the deterministic GTK fixture from a verified derived image, and runs
the installed wheel and installed sdist in separate fresh containers. The
example performs status/capability discovery, scoped launch and lease cleanup,
exact window/element resolution, semantic invoke, interpolated physical click,
Unicode strategy evidence, screenshot-on-real-failure, known-command reconnect,
stale-reference restart, and exact-origin view-only ticket issuance.
A streaming destination may have received a corrupt prefix before a
final digest failure, so use an atomic temporary destination when partial output
must never become visible. Temporary clipboard-input artifacts are deleted
best-effort only after a terminal command result; expiry remains the reliable
cleanup boundary.

## Development

From the repository root, install the reviewed, exact Linux test dependency set
into a virtual environment, then install this checkout without re-resolving its
dependencies and run every boundary:

```sh
python -m pip install --require-hashes --only-binary=:all: \
  -r packages/python/requirements-test.lock
python -m pip install --no-deps -e packages/python
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=packages/python/src \
  python -m unittest discover -s packages/python/tests -v
python scripts/conformance/run.py --adapter \
  python packages/python/scripts/run_conformance.py
(cd packages/python && mypy --cache-dir /tmp/xenoteer-mypy-cache)
ruff check --no-cache packages/python/src packages/python/tests packages/python/scripts
python -m build --outdir /tmp/xenoteer-dist packages/python
python packages/python/scripts/verify_dist.py \
  /tmp/xenoteer-dist/xenoteer-0.1.0-py3-none-any.whl \
  /tmp/xenoteer-dist/xenoteer-0.1.0.tar.gz
```

`PACKAGE_ALLOWLIST.txt` is the reviewed source-package boundary. The wheel is
configured to include only the `xenoteer` client package plus standard
distribution metadata, license, notice, and readme files. Wheel and sdist
contents are checked as exact sets; additions require an explicit allowlist
review.
