# `xenoteer`

Asyncio-first Python 3.11+ SDK for the frozen Xenoteer v1 desktop automation
API. The package is independently licensed under Apache-2.0 and contains no
Business Source License server implementation.

## Connect and control

```python
import os

from xenoteer import ClientOptions, XenoteerClient


async def automate() -> None:
    async with await XenoteerClient.connect(
        ClientOptions(
            base_url="https://127.0.0.1:9443",
            token=lambda: os.environ["XENOTEER_TOKEN"],
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

Selectors remain public v1 dictionaries so additive protocol fields are
preserved:

```python
window = await desktop.windows.one(
    {
        "type": "predicate",
        "predicate": {
            "type": "text",
            "field": "title",
            "matcher": {
                "mode": "contains",
                "value": "Editor",
                "case_sensitive": False,
            },
        },
    }
)
await (await window.activate()).wait_until_terminal(10)

element = await desktop.accessibility.one(
    {"root": {"type": "desktop"}, "predicate": {"type": "role", "role": "button"}}
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
digest; a streaming destination may have received a corrupt prefix before a
final digest failure, so use an atomic temporary destination when partial output
must never become visible. Temporary clipboard-input artifacts are deleted
best-effort only after a terminal command result; expiry remains the reliable
cleanup boundary.

## Development

From the repository root, install the reviewed, exact Linux test dependency set
into a virtual environment, then install this checkout without re-resolving its
dependencies and run every boundary:

```sh
python -m pip install -r packages/python/requirements-test.lock
python -m pip install --no-deps -e packages/python
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=packages/python/src \
  python -m unittest discover -s packages/python/tests -v
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
