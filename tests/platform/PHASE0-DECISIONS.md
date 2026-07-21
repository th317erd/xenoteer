# Phase 0 platform decisions and evidence

This file records only the mechanical decisions closed by the executable
platform spikes. It does not claim the Phase 1 actors or Phase 5 semantic cache
are implemented.

## X11 connection and extensions

- Use `x11rb 0.14.0` with the explicit `xtest`, `xkb`, `xfixes`, `damage`,
  `shm`, `composite`, `randr`, `image`, and `extra-traits` feature set. Do not
  enable `all-extensions`. The portable default does not enable
  `allow-unsafe-code`; the optional native keyboard spike enables x11rb's
  upstream-reviewed libxcb wrapper without adding first-party unsafe code.
- Every role connection performs a reply-producing core round trip and inventories
  XTEST, XKEYBOARD, XFIXES, DAMAGE, MIT-SHM, Composite, and RandR. Presence is a
  reported capability; features that require one must reject an absent extension.
- XTEST input and the QueryPointer barrier stay on the same dedicated connection.
  The live test independently received the generated MotionNotify and observed
  the exact endpoint at the barrier.
- XTEST server-side delays are validated against the shared protocol ceiling
  before coordinate conversion or any request. Exactly 10,000 ms is accepted;
  10,001 ms and `u32::MAX` return typed `DelayOutOfRange` errors.

## Observation event-loop mechanism

- Choose the dedicated poll-thread fallback, with one x11rb connection owner and
  a `mio::Waker` control FD. Do not share the connection between a Tokio
  `AsyncFd` task and reply-waiting callers.
- x11rb documents that reply processing may buffer an event even after the
  socket stops being readable. Single ownership permits `poll_for_event` to
  drain the internal queue immediately before blocking on the FD, avoiding that
  missed-readiness race.
- Shutdown sets an atomic state, wakes the poll immediately, and joins the named
  thread. Handle drop also performs best-effort stop, wake, and join rather than
  detaching. Wake failure cannot skip join; the 100 ms poll timeout is the
  bounded fallback.
- The normalized event channel has fixed capacity 256 and the X11 owner uses
  only nonblocking sends. Overflow drops/coalesces the burst behind exactly one
  `ResyncRequired` marker once capacity returns. Receiver drop wakes and
  terminates the worker. Pure 10,000-event and live 1,024-event flood tests
  enforce nonblocking and sub-second shutdown behavior.

## Pixel format

- The internal capture format is opaque, unpremultiplied BGRA8.
- Decode only core `TrueColor` visuals. `DirectColor` uses the same-looking masks
  as colormap indices rather than literal RGB bitfields, while `PseudoColor`,
  `StaticColor`, and gray classes are also colormap based; all are rejected with
  a typed unsupported-visual error instead of silently producing wrong colors.
- Derive byte order, storage bpp, scanline pad, depth, visual, and RGB masks from
  X setup/GetImage replies. Never equate depth 24 with three-byte storage.
- The authenticated Xvfb proof ran at depth 24 with 32 bits per pixel and decoded
  live red/green/blue/white/black pixels. Pure fixtures exercise both byte orders,
  reply truncation, arbitrary reply lengths, and per-channel values.
- Core GetImage is the correctness baseline. MIT-SHM remains a later parity-tested
  optimization and is not introduced by this spike.

## PNG encoding and resize seam

- Pin `png = 0.18.1` with `default-features = false`. It is a maintained,
  MIT/Apache-2.0 narrow PNG implementation, avoids a broad multi-codec image
  facade, and gives explicit RGBA8/color-depth/compression/filter control.
- Pin `fast_image_resize = 6.0.0` with `default-features = false` and only the
  `std` and `only_u8x4` features. It is maintained and MIT/Apache-2.0; restricting
  it to four-channel 8-bit pixels excludes unused pixel implementations and the
  optional `image`, Rayon, and serde dependency surfaces.
- Resize treats BGRA8 as channel-independent `U8x4`. The public seam exposes only
  nearest-neighbor (exact fixture/mask work) and Lanczos3 (screenshots), then PNG
  encoding performs an explicit BGRA-to-RGBA conversion.
- Immutable ceilings are 8192 per dimension, 16,000,000 source or destination
  pixels, and 32 MiB encoded output. Caller limits can only tighten them. Every
  pixel/byte calculation is checked, allocations use `try_reserve_exact`, and a
  bounded writer aborts PNG output at the encoded-byte ceiling.
- Core capture validates the immutable dimension and pixel ceilings before it
  sends GetImage, then RawImage enforces them again before decoding. Reply data
  must cover the computed padded scanlines and may contain no more than the
  final zero-to-three bytes required by X11's four-byte reply framing.

## Keyboard model

- Keep `xkbcommon 0.9.0` plus x11rb's libxcb-backed connection behind
  `native-xkbcommon`; default builds do not need system headers or libraries.
- The native implementation uses libxkbcommon-x11 to negotiate the extension
  and build keymap/state from the live core keyboard device, not from an assumed
  US keymap. The Xvfb proof found a nonzero keycode/layout/level mapping.
- xkbcommon's X11 bridge consumes an XCB-compatible handle, so it owns a small,
  dedicated x11rb `XCBConnection`. x11rb's safe Rust connection remains the
  injection/observation backend. The separate `xcb` Rust generator crate was
  rejected because its current releases pull advisory-affected `quick-xml 0.30`
  at build time.

## AT-SPI

- Keep `atspi 0.30.0` behind `live-atspi`, with default features disabled and
  only `connection`, `proxies`, and `tokio` enabled. This avoids an accidental
  second async runtime and keeps the portable default dependency-free.
- `AccessibilityConnection::new` correctly discovers `org.a11y.Bus` from the
  session bus, connects to the accessibility bus, and queries registry children.
- The real harness started an authenticated Xvfb and GTK3 fixture in a fresh
  `dbus-run-session`; the Rust probe found the fixture by its AT-SPI application
  name `xenoteer-atspi-fixture`.
- GTK map readiness and accessibility-bus registration are distinct. The live
  proof therefore retries registry protocol snapshots to a five-second deadline
  while checking the fixture PID through Linux process state; it does not rely
  on a fixed post-map sleep.
- One caller-supplied terminal Tokio deadline covers accessibility bus
  connection, registry-root and children fetches, every application proxy, and
  every name fetch. Snapshots reject more than 10,000 roots, names over 1,024
  UTF-8 bytes, or more than 16 MiB aggregate name data with typed timeout/limit
  errors. Root count is checked before report reservation or iteration.
- Application accessible names originate in toolkit/application metadata and
  need not equal the visible window title. Future correlation must retain both
  pieces of evidence rather than assuming equality.

## Reproduction

```sh
cargo test -p xenoteer-x11 --all-features
cargo test -p xenoteer-atspi --all-features
tests/platform/run-x11-spikes.sh
tests/platform/run-atspi-spike.sh
tests/platform/run-concurrent-spikes.sh
```

The shell harnesses fail on missing executables, X authentication/readiness,
extensions, delivery/barrier evidence, depth/bpp mismatch, keymap mapping, bus
discovery, registry discovery, or fixture identity.

Both live harnesses acquire a nonblocking per-display `flock` before checking
the X11 socket and hold it through Xvfb cleanup. The concurrent proof runs two
instances of each harness at once to exercise atomic allocation and lock release.
