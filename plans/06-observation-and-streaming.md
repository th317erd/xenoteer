# Observation, screenshots, events, and human viewing

## 1. Scope

Observation has three distinct products:

1. structured state/events for bot decisions;
2. bounded screenshots for API clients and diagnostics;
3. continuous browser viewing through RFB/noVNC.

They share the X display but not an X connection or a trust assumption. A slow
viewer must not block state observation. A delayed input request must not block
capture. Viewer input must not bypass the control lease.

## 2. Observation actor

`ObservationActor` owns an X connection selected for root/window/property and
extension events. It maintains the window model, extension inventory, pointer
observations, display generation, and monotonically increasing event sequence.

`CaptureActor` owns another connection because large GetImage replies can stall
event processing. It accepts a bounded queue of screenshot jobs and returns via
oneshot channels. Clipboard has its own event connection as defined in [05](05-keyboard-and-clipboard.md).

No actor exposes its x11rb connection as a shared `Arc<Mutex<_>>`. Typed messages
are the boundary.

## 3. Screenshot API contract

### 3.1 Capture kinds

- `root`: current visible root framebuffer region. This is the authoritative
  "what a human sees" capture, excluding the cursor unless requested.
- `window_visible`: crop/intersection of the window's current root rectangle.
  Occluding windows are included because this is root content.
- `window_drawable`: core GetImage from the client window. Only valid while
  viewable; obscured regions without backing store are undefined and the cursor
  is absent.
- `window_surface`: future Composite redirected pixmap capture, capability-gated
  and deferred until its effects on XFCE/x11vnc are proven.

Never call `window_visible` a full window screenshot. Never return cached pixels
for an unmapped/minimized target as though they are current.

### 3.2 Request

```text
ScreenshotRequest {
  kind,
  window_ref?,
  region?,
  format: png | raw_bgra,
  include_cursor: bool,
  scale?: { width?, height?, filter },
  deadline,
  max_bytes?
}
```

Release-one public default is PNG. Raw BGRA is advanced and bounded; its exact
stride/channel metadata is returned. JPEG/webp are deferred to avoid lossy
semantics and extra native dependency decisions.

Limits, initially:

- region must intersect and, by default, be fully within root bounds;
- maximum input pixels 16 million;
- maximum output dimension 8192;
- maximum encoded response 32 MiB;
- screenshot queue capacity 16;
- at most 2 in-flight encodes per desktop;
- default timeout 10 seconds.

Oversize work is rejected before allocation. Scaling preserves aspect only when
one output dimension is supplied. Filters are a closed enum; default Lanczos for
quality, nearest for pixel-exact diagnostics.

### 3.3 X11 pixel decoding

Start correctness-first with core `GetImage(ZPixmap)` on the root/drawable.
Do not assume depth 24 means 3 bytes per pixel; common servers use 32 bits per
pixel with 24-bit depth. Decode using setup pixmap formats, image byte order,
scanline pad, visual RGB masks, and returned depth/visual.

The decoder:

- validates reply size before indexing;
- handles little/big byte order declared by the server;
- normalizes to release-one internal format unpremultiplied BGRA8;
- sets alpha 255 for ordinary root visuals;
- fails unsupported visual/mask combinations explicitly;
- is fuzzed with constructed setup/reply combinations.

PNG encoding happens off the async runtime on a bounded worker pool. A timed-out
client does not leave unlimited encode work; admission owns a cancellation flag,
but already executing native/pure-Rust encode may run to completion and its
result is discarded under a semaphore bound.

### 3.4 Cursor inclusion

Core GetImage explicitly excludes the pointer cursor. With `include_cursor=true`:

1. query XFIXES cursor image/position on the capture connection after/before the
   image under a documented weak snapshot;
2. alpha-composite the cursor at position minus hotspot onto the captured region;
3. clip at region/screen boundaries;
4. report cursor sequence and whether it moved between observations if known.

This is not an atomic screen+cursor snapshot. A future synchronized capture can
use additional extension support; release one reports the limitation.

### 3.5 MIT-SHM optimization

MIT-SHM capture is a phase-two performance optimization after core capture is
correct. It requires:

- extension/version/capability probe;
- private shared segment with bounded exact size and restrictive permissions;
- robust attach/detach on success, protocol error, actor restart, and shutdown;
- no segment reuse until the server request is known complete;
- parity tests against core GetImage for every supported visual/region;
- automatic fallback with a metric, not corrupted output.

`/dev/shm` is also needed by browsers/QtWebEngine, but Xenoteer capture must not
consume it without bounds. One or two reusable buffers per capture actor are
enough initially.

## 4. DAMAGE and observation events

Select X DAMAGE on the root to learn that pixels changed. DAMAGE identifies
dirty regions; it does not contain pixels and does not guarantee each transient
frame is captured. Coalesce regions over a short frame interval, initially
16 ms, subtract acknowledged damage, and cap rectangle complexity by collapsing
to a bounding box/full-screen marker.

Uses:

- emit low-volume `screen.damaged` hints to bots;
- inform future delta screenshot streaming;
- diagnose viewer/capture freshness;
- avoid polling screenshots just to know something changed.

Do not make waits rely solely on DAMAGE. A compositor-disabled Xvfb profile
should produce predictable events, but applications and extension behavior vary.
At event queue overflow, emit `resync_required` and a full-damage marker.

## 5. WebSocket event delivery

Events from windows, AT-SPI, desktop lifecycle, actions, leases, clipboard
metadata, and screen damage enter a bounded event hub.

Envelope:

```text
{
  "type": "event",
  "protocol": "1.0",
  "desktop_id": "...",
  "desktop_generation": "0190f5a2-7b7d-7cc0-8f3e-6ddf4f1bd001",
  "sequence": 1042,
  "occurred_at": "...",
  "topic": "window.changed",
  "payload": {...}
}
```

Rules:

- Sequence is assigned after normalization, before per-client filtering.
- Clients receive sequence gaps when filtered topics occur; a gap is only an
  error when the server sends `events.dropped`/`resync_required` or sequence
  regresses.
- Each connection has one writer task and bounded high/normal queues. Lifecycle,
  command results, and resync markers are high priority; damage/motion-like
  events are coalescible normal priority.
- A slow client never backpressures X event ingestion. Coalescible events replace
  older same-key events; non-coalescible overflow closes with an application
  backpressure code after a final marker if possible.
- Reconnect requests `since_sequence`; a small bounded replay buffer may satisfy
  it. Otherwise the server returns `resync_required` and clients fetch snapshots.
- Application ping/pong verifies end-to-end responsiveness in addition to RFC
  WebSocket control frames.

Screenshots and accessibility trees are not placed inline in routine events.
Events carry resource URLs/IDs when an explicit trace artifact was captured.

## 6. Human viewing stack

### 6.1 Selected initial stack

```text
Xvfb :99
  <- x11vnc on 127.0.0.1:5900, shared, forever, server-side view-only
  <- websockify on 127.0.0.1:6080
  <- authenticated Xenoteer viewer WebSocket gateway
  <- pinned noVNC static client under /viewer/
```

x11vnc is useful and compatible but currently declares itself unmaintained. It
is an adapter, not a core daemon dependency leaked into public APIs. Define a
`ViewerBackend` supervision/config contract and conformance suite so it can be
replaced with another RFB server or native streamer.

### 6.2 Security boundary

- X11 listens on Unix socket only; x11vnc and websockify bind container loopback.
- Port 5900 and the raw websockify port are never Docker `EXPOSE`/published
  defaults.
- The viewer gateway requires the same API authentication plus `viewer:read`.
- TLS is terminated at Xenoteer or its trusted reverse proxy; browsers use WSS
  under the same origin to avoid mixed-content/certificate problems.
- The RFB hop is local to the container. RFB/VNC password authentication is not
  treated as the external security boundary; external auth is at the gateway.
- Server-side `viewonly` is mandatory. Disabling noVNC keyboard UI is defense in
  depth, not enforcement.
- Origin allowlist and short-lived, audience-bound viewer tickets prevent a URL
  from becoming a bearer session indefinitely.

Because any process running as the desktop user can already connect to the X
server, unauthenticated local-only RFB does not expand that user's effective X11
authority. If future multi-user isolation changes that assumption, require
per-session RFB credentials or a Unix-socket-capable backend.

### 6.3 Viewer behavior

Pin a noVNC release and serve immutable hashed assets. Configure:

- view-only at client and server;
- local cursor rendering tested against x11vnc XFIXES cursor updates;
- resize scaling in browser only—never request desktop RandR resize in release
  one;
- clipboard UI disabled by default because it bypasses Xenoteer's clipboard
  auditing/preservation policy;
- file transfer disabled;
- reconnect bounded with clear generation/session state;
- quality/compression profiles selectable by operator, not bot action API.

Viewer tickets identify desktop generation. A restarted container/desktop forces
a new ticket; noVNC must not show a stale framebuffer as live.

### 6.4 x11vnc settings and gotchas

Required semantic settings (exact supported flags verified against pinned build):

- fixed display/auth file;
- loopback bind and fixed internal RFB port;
- `viewonly`, `forever`, and shared viewers;
- no file transfer, remote command/control, desktop resize, or external HTTP
  listener;
- X DAMAGE enabled normally;
- XFIXES cursor support enabled;
- bounded client count and disconnect cleanup.

`-noxdamage` is only a compatibility fallback when a conformance test proves the
pinned Xvfb/build produces missing updates. It increases polling/CPU and is
reported in status. x11vnc threaded modes have upstream crash caveats; use the
simplest tested mode and soak it under multiple viewers.

## 7. Viewer takeover (future-compatible release-one behavior)

Release one exposes view-only tickets only. Protocol schema reserves a
`control_mode_generation` and emits `control.mode_changed`, but no endpoint turns
RFB input on.

When takeover is later implemented, it must be a server transaction:

```text
BotControlled -> Revoking -> Resetting -> HumanControlled
HumanControlled -> DisablingRfbInput -> Resetting -> BotControlled
```

The gateway/UI is not allowed to toggle locally without daemon confirmation.
Viewer disconnection in human mode triggers input disable/reset before a bot
lease can be granted.

## 8. Artifact capture and privacy

Action traces may attach screenshots only under policy:

- `never` default for server-wide operation;
- `on_failure` common test profile;
- `before_after` explicit debugging;
- `manual` per request.

Artifacts have byte/count/retention limits, randomized opaque IDs, authorization,
content type, hash, creation/expiry, and redaction metadata. They are stored only
in a configured directory/volume or external sink. Logs contain artifact IDs,
not bytes or guessed OCR content.

## 9. Failure handling

- Capture queue full -> `resource_exhausted`, never unlimited buffering.
- Invalid drawable/target vanished -> typed failure with last window snapshot.
- Unsupported visual -> `capture_format_unsupported`.
- PNG output exceeds limit -> abort/discard and return `artifact_too_large`.
- x11vnc/websockify failure -> viewer degraded/restarting; bot control remains
  ready unless viewing is configured required.
- Xvfb failure -> desktop generation invalid; all observation/capture/viewer
  sessions terminate.
- Event client slow -> coalesce/drop marker/close; never stall actor.
- Viewer gateway loses upstream -> close noVNC WebSocket with restart reason;
  do not loop proxy reconnect invisibly across generations.

## 10. Tests and performance budgets

### Capture correctness

- color bars covering visual masks and byte-order paths;
- full root, edge crops, one pixel, invalid/empty/oversize regions;
- overlapping windows distinguish root/window_visible/window_drawable;
- minimized/unmapped window errors;
- cursor hotspot/alpha/edge clipping and cursor-excluded default;
- PNG decode round trip and raw stride metadata;
- core versus MIT-SHM parity;
- compositor-off transparent/ARGB window fixture.

### Eventing

- check/subscribe race, reconnect replay, overflow/resync, generation change;
- high-priority command result survives damage flood;
- one writer per WebSocket and orderly close/heartbeat;
- sequence monotonic under concurrent X11 and AT-SPI events.

### Viewer

- scan listeners: only intended external API port;
- raw RFB/websockify unreachable outside container loopback;
- keyboard/pointer/clipboard attempts from noVNC do not reach X event recorder;
- two simultaneous viewers, reconnect, slow network, cursor updates;
- x11vnc crash/restart and stale-generation close;
- 8-hour viewer soak with bounded RSS/CPU.

Initial performance gates on a documented CI host:

- 1920x1080 root GetImage+PNG p95 under 250 ms;
- 100x100 crop p95 under 30 ms;
- observation event loop p99 processing lag under 50 ms during one full-screen
  capture per second;
- viewer idle CPU under 5% of one core and no unbounded RSS growth.

Budgets are measured and revised with evidence; they are not cross-hardware
guarantees.

## 11. Primary references

- [X11 GetImage behavior](https://www.x.org/archive/X11R7.5/doc/man/man3/XGetImage.3.html)
- [X11 core protocol GetImage](https://www.x.org/releases/X11R7.6/doc/xproto/x11protocol.pdf)
- [MIT-SHM extension](https://www.x.org/releases/X11R7.5/doc/Xext/mit-shm.pdf)
- [X11R7.7 extension specifications index](https://www.x.org/releases/X11R7.7/doc/)
- [X Composite library behavior](https://www.x.org/archive/X11R7.5/doc/man/man3/Xcomposite.3.html)
- [x11rb image/extension support](https://docs.rs/x11rb/latest/x11rb/)
- [x11vnc upstream and maintenance status](https://github.com/LibVNC/x11vnc)
- [x11vnc option reference](https://github.com/LibVNC/x11vnc/blob/master/doc/OPTIONS.md)
- [noVNC upstream](https://github.com/novnc/noVNC)
- [noVNC integration API](https://github.com/novnc/noVNC/blob/master/docs/API.md)
- [websockify upstream](https://github.com/novnc/websockify)
- [WebSocket protocol RFC 6455](https://www.rfc-editor.org/info/rfc6455/)
